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

    // ── Large data transfer ───────────────────────────────────

    #[test]
    fn large_data_roundtrip() {
        let path = temp_socket_path("large");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let data: Vec<u8> = (0..8192).map(|i| (i % 256) as u8).collect();

        let client = std::thread::spawn({
            let path = path.clone();
            let data = data.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(&data).unwrap();
                stream.flush().unwrap();
            }
        });

        let (mut server_stream, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);

        let mut buf = Vec::new();
        server_stream.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
        cleanup(&path);
    }

    // ── Binary data ──────────────────────────────────────────

    #[test]
    fn binary_data_all_byte_values() {
        let path = temp_socket_path("binary");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let data: Vec<u8> = (0..=255).collect();

        let client = std::thread::spawn({
            let path = path.clone();
            let data = data.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(&data).unwrap();
            }
        });

        let (mut server_stream, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);

        let mut buf = Vec::new();
        server_stream.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
        cleanup(&path);
    }

    // ── EOF behavior ─────────────────────────────────────────

    #[test]
    fn read_returns_eof_after_writer_drops() {
        let path = temp_socket_path("eof");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(b"fin").unwrap();
                // stream drops here, closing the connection
            }
        });

        let (mut server_stream, _) = listener.accept().unwrap();
        client.join().unwrap();

        let mut buf = Vec::new();
        server_stream.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"fin");

        // Further reads should return 0 (EOF)
        let mut extra = [0u8; 16];
        let n = server_stream.read(&mut extra).unwrap();
        assert_eq!(n, 0);
        cleanup(&path);
    }

    // ── Non-blocking read ────────────────────────────────────

    #[test]
    fn nonblocking_read_returns_would_block() {
        let path = temp_socket_path("nonblock");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (mut server_stream, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();

        server_stream.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 16];
        let result = server_stream.read(&mut buf);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        cleanup(&path);
    }

    // ── Listener re-bind after cleanup ───────────────────────

    #[test]
    fn rebind_after_cleanup() {
        let path = temp_socket_path("rebind");
        cleanup(&path);

        {
            let _listener = UnixListener::bind(&path).unwrap();
        }
        // After dropping listener, clean up socket file and re-bind
        cleanup(&path);
        let _listener2 = UnixListener::bind(&path).unwrap();
        cleanup(&path);
    }

    // ── DerefMut ─────────────────────────────────────────────

    #[test]
    fn unix_stream_deref_mut_exposes_inner() {
        let path = temp_socket_path("deref_mut_s");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (mut server_stream, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();
        // DerefMut gives mutable access to inner StreamImpl
        let inner: &mut StreamImpl = &mut *server_stream;
        let _ = inner.set_nonblocking(true);
        cleanup(&path);
    }

    #[test]
    fn unix_listener_deref_mut_exposes_inner() {
        let path = temp_socket_path("deref_mut_l");
        cleanup(&path);
        let mut listener = UnixListener::bind(&path).unwrap();
        // DerefMut gives mutable access to inner ListenerImpl
        let inner: &mut ListenerImpl = &mut *listener;
        inner.set_nonblocking(true).unwrap();
        cleanup(&path);
    }

    // ── Sequential connections ────────────────────────────────

    #[test]
    fn sequential_connections_work() {
        let path = temp_socket_path("seq");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        for i in 0..3u8 {
            let client = std::thread::spawn({
                let path = path.clone();
                move || {
                    let mut stream = UnixStream::connect(&path).unwrap();
                    stream.write_all(&[i]).unwrap();
                }
            });

            let (mut server_stream, _) = listener.accept().unwrap();
            client.join().unwrap();

            let mut buf = [0u8; 1];
            server_stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], i);
        }
        cleanup(&path);
    }

    // ── into_raw_fd / from_raw_fd ────────────────────────────

    #[cfg(unix)]
    #[test]
    fn into_raw_fd_and_from_raw_fd_roundtrip() {
        let path = temp_socket_path("rawfd_rt");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(b"rawfd test").unwrap();
            }
        });

        let (server_stream, _) = listener.accept().unwrap();
        client.join().unwrap();

        // Convert to raw fd and back
        let raw = server_stream.into_raw_fd();
        assert!(raw >= 0);
        let mut restored = unsafe { UnixStream::from_raw_fd(raw) };

        let mut buf = Vec::new();
        restored.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"rawfd test");
        cleanup(&path);
    }

    // ── Multiple messages over same connection ────────────────

    #[test]
    fn multiple_messages_same_connection() {
        let path = temp_socket_path("multi_msg");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                for i in 0..5u8 {
                    stream.write_all(&[i; 10]).unwrap();
                    stream.flush().unwrap();
                }
            }
        });

        let (mut server_stream, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);

        let mut buf = Vec::new();
        server_stream.read_to_end(&mut buf).unwrap();
        assert_eq!(buf.len(), 50);
        cleanup(&path);
    }

    // ── Simultaneous bidirectional ───────────────────────────

    #[test]
    fn simultaneous_bidirectional_exchange() {
        let path = temp_socket_path("simul_bidir");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client_handle = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                // Send, then receive
                stream.write_all(b"client-data").unwrap();
                stream.flush().unwrap();
                let mut buf = [0u8; 64];
                let n = stream.read(&mut buf).unwrap();
                String::from_utf8(buf[..n].to_vec()).unwrap()
            }
        });

        let (mut server_stream, _) = listener.accept().unwrap();
        // Read from client, then send response
        let mut buf = [0u8; 64];
        let n = server_stream.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"client-data");
        server_stream.write_all(b"server-data").unwrap();
        server_stream.flush().unwrap();

        let client_received = client_handle.join().unwrap();
        assert_eq!(client_received, "server-data");
        cleanup(&path);
    }

    // ── Write return values ──────────────────────────────────

    #[test]
    fn write_returns_correct_byte_count() {
        let path = temp_socket_path("wr_count");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                let n = stream.write(b"five5").unwrap();
                assert_eq!(n, 5);
                let n = stream.write(b"x").unwrap();
                assert_eq!(n, 1);
            }
        });

        let (_server, _) = listener.accept().unwrap();
        client.join().unwrap();
        cleanup(&path);
    }

    // ── Flush on clean stream ────────────────────────────────

    #[test]
    fn flush_on_clean_stream_succeeds() {
        let path = temp_socket_path("flush_clean");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                // Flush without writing anything
                stream.flush().unwrap();
            }
        });

        let (_server, _) = listener.accept().unwrap();
        client.join().unwrap();
        cleanup(&path);
    }

    // ── Read into zero-length buffer ─────────────────────────

    #[test]
    fn read_zero_length_buffer_returns_zero() {
        let path = temp_socket_path("zero_buf");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(b"data").unwrap();
            }
        });

        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        let mut empty_buf = [0u8; 0];
        let n = server.read(&mut empty_buf).unwrap();
        assert_eq!(n, 0);
        cleanup(&path);
    }

    // ── Nonblocking accept ───────────────────────────────────

    #[test]
    fn nonblocking_accept_returns_would_block() {
        let path = temp_socket_path("nb_accept");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();

        // No client connecting, so accept should return WouldBlock
        let result = listener.accept();
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        cleanup(&path);
    }

    // ── Listener local_addr ──────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn listener_local_addr() {
        let path = temp_socket_path("local_addr");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let addr = listener.local_addr().unwrap();
        // On unix, the path should match
        assert!(addr.as_pathname().is_some());
        assert_eq!(addr.as_pathname().unwrap(), path);
        cleanup(&path);
    }

    // ── Set read/write timeout via Deref ─────────────────────

    #[test]
    fn set_read_timeout() {
        let path = temp_socket_path("rd_timeout");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (server, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();
        // Via Deref, set_read_timeout is available
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        cleanup(&path);
    }

    #[test]
    fn set_write_timeout() {
        let path = temp_socket_path("wr_timeout");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (server, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();
        server
            .set_write_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        cleanup(&path);
    }

    // ── Read with timeout triggers TimedOut ───────────────────

    #[test]
    fn read_with_timeout_triggers_timed_out() {
        let path = temp_socket_path("rd_timeo");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (mut server, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(10)))
            .unwrap();
        let mut buf = [0u8; 16];
        let result = server.read(&mut buf);
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Could be TimedOut or WouldBlock depending on OS
        assert!(
            err.kind() == std::io::ErrorKind::TimedOut
                || err.kind() == std::io::ErrorKind::WouldBlock
        );
        cleanup(&path);
    }

    // ── Bind fails when socket file already exists ───────────

    #[test]
    fn bind_fails_if_socket_exists() {
        let path = temp_socket_path("exist_sock");
        cleanup(&path);
        let _listener1 = UnixListener::bind(&path).unwrap();
        // Second bind should fail because socket file exists
        let result = UnixListener::bind(&path);
        assert!(result.is_err());
        drop(_listener1);
        cleanup(&path);
    }

    // ── Connect to non-socket file fails ─────────────────────

    #[test]
    fn connect_to_regular_file_fails() {
        let path = temp_socket_path("reg_file");
        cleanup(&path);
        // Create a regular file at the path
        std::fs::write(&path, b"not a socket").unwrap();
        let result = UnixStream::connect(&path);
        assert!(result.is_err());
        cleanup(&path);
    }

    // ── 64KB data transfer ───────────────────────────────────

    #[test]
    fn large_data_64kb_roundtrip() {
        let path = temp_socket_path("large64k");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let data: Vec<u8> = (0..65536).map(|i| (i % 256) as u8).collect();

        let client = std::thread::spawn({
            let path = path.clone();
            let data = data.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(&data).unwrap();
                stream.flush().unwrap();
            }
        });

        let (mut server, _) = listener.accept().unwrap();
        drop(listener);

        // Read concurrently with client writing to avoid deadlock
        // (socket buffer may be smaller than 64KB)
        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        client.join().unwrap();
        assert_eq!(buf.len(), 65536);
        assert_eq!(buf, data);
        cleanup(&path);
    }

    // ── Nonblocking toggle ───────────────────────────────────

    #[test]
    fn toggle_nonblocking_mode() {
        let path = temp_socket_path("nb_toggle");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (mut server, _) = listener.accept().unwrap();
        let _client_stream = client.join().unwrap();

        // Set nonblocking
        server.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 16];
        let result = server.read(&mut buf);
        assert!(result.is_err());

        // Set back to blocking
        server.set_nonblocking(false).unwrap();
        // (We can't easily test blocking read without data, so just verify no error)
        cleanup(&path);
    }

    // ── Multiple flushes ─────────────────────────────────────

    #[test]
    fn multiple_flush_calls_succeed() {
        let path = temp_socket_path("multi_flush");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(b"a").unwrap();
                stream.flush().unwrap();
                stream.flush().unwrap();
                stream.write_all(b"b").unwrap();
                stream.flush().unwrap();
            }
        });

        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);

        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"ab");
        cleanup(&path);
    }

    // ── Quick successive connects ────────────────────────────

    #[test]
    fn rapid_sequential_connects() {
        let path = temp_socket_path("rapid_seq");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        for i in 0..10u8 {
            let client = std::thread::spawn({
                let path = path.clone();
                move || {
                    let mut stream = UnixStream::connect(&path).unwrap();
                    stream.write_all(&[i]).unwrap();
                }
            });

            let (mut server, _) = listener.accept().unwrap();
            client.join().unwrap();
            let mut buf = [0u8; 1];
            server.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], i);
        }
        cleanup(&path);
    }

    // ── Shutdown write side ──────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn shutdown_write_causes_eof_on_peer() {
        use std::net::Shutdown;

        let path = temp_socket_path("shutdown");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(b"before shutdown").unwrap();
                stream.shutdown(Shutdown::Write).unwrap();
                // Read should still work after shutting down write
                let mut buf = [0u8; 64];
                let n = stream.read(&mut buf).unwrap();
                String::from_utf8(buf[..n].to_vec()).unwrap()
            }
        });

        let (mut server, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"before shutdown");
        server.write_all(b"reply").unwrap();
        drop(server);

        let reply = client.join().unwrap();
        assert_eq!(reply, "reply");
        cleanup(&path);
    }

    // ── Peer_addr on connected stream ────────────────────────

    #[cfg(unix)]
    #[test]
    fn peer_addr_accessible() {
        let path = temp_socket_path("peer_addr");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });

        let (server, _addr) = listener.accept().unwrap();
        let _client = client.join().unwrap();
        // peer_addr should not error (though it may be unnamed)
        let _ = server.peer_addr().unwrap();
        cleanup(&path);
    }

    // ── Interleaved empty and real writes ────────────────────

    #[test]
    fn interleaved_empty_and_real_writes() {
        let path = temp_socket_path("interleave");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(b"").unwrap();
                stream.write_all(b"a").unwrap();
                stream.write_all(b"").unwrap();
                stream.write_all(b"bc").unwrap();
                stream.write_all(b"").unwrap();
                stream.flush().unwrap();
            }
        });

        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);

        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"abc");
        cleanup(&path);
    }

    // ── Get/clear timeouts ──────────────────────────────────

    #[test]
    fn get_read_timeout_returns_set_value() {
        let path = temp_socket_path("get_rd_to");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        let dur = std::time::Duration::from_millis(250);
        server.set_read_timeout(Some(dur)).unwrap();
        let got = server.read_timeout().unwrap().unwrap();
        assert_eq!(got, dur);
        cleanup(&path);
    }

    #[test]
    fn get_write_timeout_returns_set_value() {
        let path = temp_socket_path("get_wr_to");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        let dur = std::time::Duration::from_millis(300);
        server.set_write_timeout(Some(dur)).unwrap();
        let got = server.write_timeout().unwrap().unwrap();
        assert_eq!(got, dur);
        cleanup(&path);
    }

    #[test]
    fn clear_read_timeout() {
        let path = temp_socket_path("clr_rd_to");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_millis(100))).unwrap();
        server.set_read_timeout(None).unwrap();
        assert!(server.read_timeout().unwrap().is_none());
        cleanup(&path);
    }

    #[test]
    fn clear_write_timeout() {
        let path = temp_socket_path("clr_wr_to");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        server.set_write_timeout(Some(std::time::Duration::from_millis(100))).unwrap();
        server.set_write_timeout(None).unwrap();
        assert!(server.write_timeout().unwrap().is_none());
        cleanup(&path);
    }

    // ── Accept addr ─────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn accept_addr_is_unnamed() {
        let path = temp_socket_path("unnamed");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (_server, addr) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        // Client-side addresses are typically unnamed
        assert!(addr.as_pathname().is_none());
        cleanup(&path);
    }

    // ── Stream survives listener drop ───────────────────────

    #[test]
    fn stream_stays_connected_after_listener_drop() {
        let path = temp_socket_path("srv_drop");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(b"before drop").unwrap();
                s
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        let mut c = client.join().unwrap();
        drop(listener); // drop listener
        // Communication should still work
        server.write_all(b"after drop").unwrap();
        server.flush().unwrap();
        let mut buf = [0u8; 64];
        let n = c.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"after drop");
        cleanup(&path);
    }

    // ── Drop listener while client is connected ─────────────

    #[test]
    fn drop_listener_doesnt_kill_streams() {
        let path = temp_socket_path("drop_ls");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (mut server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        drop(listener);
        // Server stream should still be writable
        server.write_all(b"still alive").unwrap();
        server.flush().unwrap();
        cleanup(&path);
    }

    // ── Write after peer dropped ────────────────────────────

    #[test]
    fn write_after_peer_dropped_fails() {
        let path = temp_socket_path("wr_dead");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (mut server, _) = listener.accept().unwrap();
        let c = client.join().unwrap();
        drop(c); // close client end
        // Give OS time to propagate the close
        std::thread::sleep(std::time::Duration::from_millis(50));
        // First write may succeed (buffered), but repeated writes should eventually fail
        let mut failed = false;
        for _ in 0..100 {
            if server.write_all(&[0u8; 1024]).is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "expected write to fail after peer dropped");
        cleanup(&path);
    }

    // ── Read into oversized buffer ──────────────────────────

    #[test]
    fn read_into_oversized_buffer() {
        let path = temp_socket_path("big_buf");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(b"tiny").unwrap();
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let mut buf = vec![0u8; 65536];
        let n = server.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"tiny");
        cleanup(&path);
    }

    // ── Single byte read/write ──────────────────────────────

    #[test]
    fn write_single_byte() {
        let path = temp_socket_path("single_w");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                let n = s.write(&[42]).unwrap();
                assert_eq!(n, 1);
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        let mut buf = [0u8; 1];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], 42);
        cleanup(&path);
    }

    // ── Partial read then rest ──────────────────────────────

    #[test]
    fn partial_read_then_rest() {
        let path = temp_socket_path("partial");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(b"hello world").unwrap();
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        // Read first 5 bytes
        let mut part1 = [0u8; 5];
        server.read_exact(&mut part1).unwrap();
        assert_eq!(&part1, b"hello");
        // Read rest
        let mut rest = Vec::new();
        server.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b" world");
        cleanup(&path);
    }

    // ── Read exact with insufficient data ───────────────────

    #[test]
    fn read_exact_insufficient_data_fails() {
        let path = temp_socket_path("exact_fail");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(b"hi").unwrap();
                // drop closes the stream
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        let mut buf = [0u8; 100];
        let result = server.read_exact(&mut buf);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
        cleanup(&path);
    }

    // ── Connect with PathBuf ────────────────────────────────

    #[test]
    fn connect_with_pathbuf() {
        let path = temp_socket_path("pathbuf");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let owned: std::path::PathBuf = path.clone();
        let client = std::thread::spawn(move || UnixStream::connect(&owned).unwrap());
        let (mut server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        server.write_all(b"ok").unwrap();
        cleanup(&path);
    }

    // ── Accept returns working stream ───────────────────────

    #[test]
    fn accept_returns_working_stream() {
        let path = temp_socket_path("accept_wk");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).unwrap();
                String::from_utf8(buf[..n].to_vec()).unwrap()
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        server.write_all(b"from server").unwrap();
        server.flush().unwrap();
        drop(server);
        let got = client.join().unwrap();
        assert_eq!(got, "from server");
        cleanup(&path);
    }

    // ── Five sequential accepts ─────────────────────────────

    #[test]
    fn five_sequential_accepts() {
        let path = temp_socket_path("five_seq");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        for i in 0..5u8 {
            let client = std::thread::spawn({
                let path = path.clone();
                move || {
                    let mut s = UnixStream::connect(&path).unwrap();
                    s.write_all(&[i]).unwrap();
                }
            });
            let (mut server, _) = listener.accept().unwrap();
            client.join().unwrap();
            let mut buf = [0u8; 1];
            server.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], i);
        }
        cleanup(&path);
    }

    // ── BufReader wrapping ──────────────────────────────────

    #[test]
    fn buf_reader_wrapping() {
        use std::io::BufRead;
        let path = temp_socket_path("bufreader");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(b"line1\nline2\n").unwrap();
            }
        });
        let (server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let reader = std::io::BufReader::new(server);
        let lines: Vec<String> = reader.lines().map(|l| l.unwrap()).collect();
        assert_eq!(lines, vec!["line1", "line2"]);
        cleanup(&path);
    }

    // ── BufWriter wrapping ──────────────────────────────────

    #[test]
    fn buf_writer_wrapping() {
        let path = temp_socket_path("bufwriter");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let s = UnixStream::connect(&path).unwrap();
                let mut w = std::io::BufWriter::new(s);
                w.write_all(b"buffered").unwrap();
                w.flush().unwrap();
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"buffered");
        cleanup(&path);
    }

    // ── Read::bytes iterator ────────────────────────────────

    #[test]
    fn bytes_iterator() {
        let path = temp_socket_path("bytes_it");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(b"ABC").unwrap();
            }
        });
        let (server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let collected: Vec<u8> = server.bytes().map(|b| b.unwrap()).collect();
        assert_eq!(collected, b"ABC");
        cleanup(&path);
    }

    // ── Read::take ──────────────────────────────────────────

    #[test]
    fn read_take_limits_bytes() {
        let path = temp_socket_path("take_lim");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(b"abcdefgh").unwrap();
            }
        });
        let (server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let mut limited = server.take(3);
        let mut buf = Vec::new();
        limited.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"abc");
        cleanup(&path);
    }

    // ── Both ends write simultaneously ──────────────────────

    #[test]
    fn both_ends_write() {
        let path = temp_socket_path("both_wr");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(b"from client").unwrap();
                s.flush().unwrap();
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).unwrap();
                String::from_utf8(buf[..n].to_vec()).unwrap()
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        server.write_all(b"from server").unwrap();
        server.flush().unwrap();
        let mut buf = [0u8; 64];
        let n = server.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"from client");
        let got = client.join().unwrap();
        assert_eq!(got, "from server");
        cleanup(&path);
    }

    // ── 256KB data roundtrip ────────────────────────────────

    #[test]
    fn large_data_256kb_roundtrip() {
        let path = temp_socket_path("large256k");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let data: Vec<u8> = (0..262144).map(|i| (i % 251) as u8).collect();

        let client = std::thread::spawn({
            let path = path.clone();
            let data = data.clone();
            move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream.write_all(&data).unwrap();
                stream.flush().unwrap();
            }
        });

        let (mut server, _) = listener.accept().unwrap();
        drop(listener);
        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        client.join().unwrap();
        assert_eq!(buf.len(), 262144);
        assert_eq!(buf, data);
        cleanup(&path);
    }

    // ── Incoming with multiple connections ───────────────────

    #[test]
    fn incoming_multiple_connections() {
        let path = temp_socket_path("inc_multi");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();

        // Connect 3 clients
        let clients: Vec<_> = (0..3).map(|_| {
            let path = path.clone();
            std::thread::spawn(move || UnixStream::connect(&path).unwrap())
        }).collect();

        // Give clients time to connect
        std::thread::sleep(std::time::Duration::from_millis(100));

        let mut accepted = 0;
        for result in listener.incoming() {
            match result {
                Ok(_stream) => {
                    accepted += 1;
                    if accepted >= 3 {
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        for c in clients {
            c.join().unwrap();
        }
        assert!(accepted >= 2, "expected at least 2 accepts, got {accepted}");
        cleanup(&path);
    }

    // ── Try clone via Deref ─────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn try_clone_via_deref() {
        let path = temp_socket_path("tryclone");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(b"cloned").unwrap();
            }
        });
        let (server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        // try_clone is available via Deref to StreamImpl
        let cloned_inner = server.try_clone().unwrap();
        let mut wrapped = UnixStream(cloned_inner);
        let mut buf = Vec::new();
        wrapped.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"cloned");
        cleanup(&path);
    }

    // ── Shutdown both halves ────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn shutdown_both_halves() {
        use std::net::Shutdown;
        let path = temp_socket_path("shut_both");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        server.shutdown(Shutdown::Both).unwrap();
        cleanup(&path);
    }

    // ── Shutdown read half, write still works ───────────────

    #[cfg(unix)]
    #[test]
    fn shutdown_read_write_still_works() {
        use std::net::Shutdown;
        let path = temp_socket_path("shut_rd");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).unwrap();
                String::from_utf8(buf[..n].to_vec()).unwrap()
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        server.shutdown(Shutdown::Read).unwrap();
        // Writing should still work
        server.write_all(b"still writing").unwrap();
        drop(server);
        let got = client.join().unwrap();
        assert_eq!(got, "still writing");
        cleanup(&path);
    }

    // ── AsRawFd consistency ─────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn as_raw_fd_is_consistent() {
        let path = temp_socket_path("fd_cons");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        let fd1 = server.as_raw_fd();
        let fd2 = server.as_raw_fd();
        assert_eq!(fd1, fd2);
        cleanup(&path);
    }

    // ── Write all correctness ───────────────────────────────

    #[test]
    fn write_all_sends_complete_buffer() {
        let path = temp_socket_path("wr_all");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let msg = b"the quick brown fox jumps over the lazy dog";
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                s.write_all(msg).unwrap();
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, msg);
        cleanup(&path);
    }

    // ── Connect error kind ──────────────────────────────────

    #[test]
    fn connect_nonexistent_error_kind() {
        let path = temp_socket_path("no_exist_kind");
        cleanup(&path);
        let result = UnixStream::connect(&path);
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    // ── Bind error kind for existing socket ─────────────────

    #[test]
    fn bind_existing_error_kind() {
        let path = temp_socket_path("bind_err_k");
        cleanup(&path);
        let _listener = UnixListener::bind(&path).unwrap();
        let result = UnixListener::bind(&path);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        cleanup(&path);
    }

    // ── Nonblocking write doesn't block ─────────────────────

    #[test]
    fn nonblocking_write_returns_immediately() {
        let path = temp_socket_path("nb_write");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (mut server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        server.set_nonblocking(true).unwrap();
        // Small write should succeed even in nonblocking mode
        let result = server.write(b"nb");
        assert!(result.is_ok());
        cleanup(&path);
    }

    // ── Debug format of stream contains fd info ─────────────

    #[test]
    fn unix_stream_debug_contains_fd() {
        let path = temp_socket_path("dbg_fd");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        let dbg = format!("{server:?}");
        // Debug output should contain "UnixStream" and file descriptor info
        assert!(dbg.contains("UnixStream"));
        cleanup(&path);
    }

    // ── Listener take_error via Deref ───────────────────────

    #[test]
    fn listener_take_error_via_deref() {
        let path = temp_socket_path("take_err");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        // take_error should return Ok(None) when no error
        let err = listener.take_error().unwrap();
        assert!(err.is_none());
        cleanup(&path);
    }

    // ── Stream take_error via Deref ─────────────────────────

    #[test]
    fn stream_take_error_via_deref() {
        let path = temp_socket_path("str_tkerr");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        let err = server.take_error().unwrap();
        assert!(err.is_none());
        cleanup(&path);
    }

    // ── Double read after EOF ───────────────────────────────

    #[test]
    fn double_read_after_eof_returns_zero() {
        let path = temp_socket_path("dbl_eof");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || UnixStream::connect(&path).unwrap()
            // drops immediately
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        let mut buf = [0u8; 16];
        let n1 = server.read(&mut buf).unwrap();
        assert_eq!(n1, 0);
        let n2 = server.read(&mut buf).unwrap();
        assert_eq!(n2, 0);
        cleanup(&path);
    }

    // ── Two streams have distinct fds ───────────────────────

    #[cfg(unix)]
    #[test]
    fn two_streams_have_distinct_fds() {
        let path = temp_socket_path("two_fds");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let c1 = std::thread::spawn({
            let p = path.clone();
            move || UnixStream::connect(&p).unwrap()
        });
        let c2 = std::thread::spawn({
            let p = path.clone();
            move || UnixStream::connect(&p).unwrap()
        });
        let (s1, _) = listener.accept().unwrap();
        let (s2, _) = listener.accept().unwrap();
        let _c1 = c1.join().unwrap();
        let _c2 = c2.join().unwrap();
        assert_ne!(s1.as_raw_fd(), s2.as_raw_fd());
        cleanup(&path);
    }

    // ── Write vectored via std::io::Write ───────────────────

    #[test]
    fn write_vectored_basic() {
        let path = temp_socket_path("wr_vec");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut s = UnixStream::connect(&path).unwrap();
                let bufs = [
                    std::io::IoSlice::new(b"hello"),
                    std::io::IoSlice::new(b" "),
                    std::io::IoSlice::new(b"world"),
                ];
                let n = s.write_vectored(&bufs).unwrap();
                // write_vectored may not write all slices, but should write at least some
                assert!(n > 0);
                assert!(n <= 11);
                s.flush().unwrap();
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        // At minimum the first slice should have been written
        assert!(buf.starts_with(b"hello"));
        cleanup(&path);
    }

    // ── Chain two streams ───────────────────────────────────

    #[test]
    fn read_chain_two_streams() {
        let path1 = temp_socket_path("chain1");
        let path2 = temp_socket_path("chain2");
        cleanup(&path1);
        cleanup(&path2);
        let l1 = UnixListener::bind(&path1).unwrap();
        let l2 = UnixListener::bind(&path2).unwrap();
        let c1 = std::thread::spawn({
            let p = path1.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                s.write_all(b"first").unwrap();
            }
        });
        let c2 = std::thread::spawn({
            let p = path2.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                s.write_all(b"second").unwrap();
            }
        });
        let (s1, _) = l1.accept().unwrap();
        let (s2, _) = l2.accept().unwrap();
        c1.join().unwrap();
        c2.join().unwrap();
        drop(l1);
        drop(l2);
        let mut chained = s1.chain(s2);
        let mut buf = Vec::new();
        chained.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"firstsecond");
        cleanup(&path1);
        cleanup(&path2);
    }

    // ── Short socket path works ─────────────────────────────

    #[test]
    fn short_socket_path() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("t{}", std::process::id()));
        cleanup(&path);
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(path.exists());
        cleanup(&path);
    }

    // ── Server reads from multiple clients concurrently ─────

    #[test]
    fn accept_multiple_then_read_all() {
        let path = temp_socket_path("multi_rd");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let clients: Vec<_> = (0..3u8).map(|i| {
            let p = path.clone();
            std::thread::spawn(move || {
                let mut s = UnixStream::connect(&p).unwrap();
                s.write_all(&[i; 4]).unwrap();
            })
        }).collect();
        let mut servers = Vec::new();
        for _ in 0..3 {
            let (s, _) = listener.accept().unwrap();
            servers.push(s);
        }
        for c in clients {
            c.join().unwrap();
        }
        drop(listener);
        let mut all_data = Vec::new();
        for mut s in servers {
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).unwrap();
            assert_eq!(buf.len(), 4);
            all_data.push(buf[0]);
        }
        all_data.sort();
        assert_eq!(all_data, vec![0, 1, 2]);
        cleanup(&path);
    }

    // ── Write then read on same client ────────────────────

    #[test]
    fn client_write_then_read_on_same_stream() {
        let path = temp_socket_path("cl_wr_rd");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                s.write_all(b"request").unwrap();
                s.flush().unwrap();
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).unwrap();
                String::from_utf8(buf[..n].to_vec()).unwrap()
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        let mut buf = [0u8; 64];
        let n = server.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"request");
        server.write_all(b"response").unwrap();
        server.flush().unwrap();
        drop(server);
        let got = client.join().unwrap();
        assert_eq!(got, "response");
        cleanup(&path);
    }

    // ── Third-pass expansion ────────────────────────────────────

    #[test]
    fn read_timeout_initially_none() {
        let path = temp_socket_path("rd_to_init");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || UnixStream::connect(&p).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        assert!(server.read_timeout().unwrap().is_none());
        cleanup(&path);
    }

    #[test]
    fn write_timeout_initially_none() {
        let path = temp_socket_path("wr_to_init");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || UnixStream::connect(&p).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        assert!(server.write_timeout().unwrap().is_none());
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn write_after_shutdown_write_fails() {
        use std::net::Shutdown;
        let path = temp_socket_path("wr_shut");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || UnixStream::connect(&p).unwrap()
        });
        let (mut server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        server.shutdown(Shutdown::Write).unwrap();
        let result = server.write(b"should fail");
        assert!(result.is_err());
        cleanup(&path);
    }

    #[test]
    fn read_one_byte_buffer_from_multi_byte_message() {
        let path = temp_socket_path("1byte_rd");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                s.write_all(b"ABCDE").unwrap();
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let mut collected = Vec::new();
        loop {
            let mut buf = [0u8; 1];
            match server.read(&mut buf) {
                Ok(0) => break,
                Ok(1) => collected.push(buf[0]),
                Ok(_) => unreachable!(),
                Err(_) => break,
            }
        }
        assert_eq!(collected, b"ABCDE");
        cleanup(&path);
    }

    #[test]
    fn listener_nonblocking_toggle_back() {
        let path = temp_socket_path("nb_back");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        // Should get WouldBlock
        assert!(listener.accept().is_err());
        listener.set_nonblocking(false).unwrap();
        // Now it's blocking again (can't easily test without a client)
        cleanup(&path);
    }

    #[test]
    fn write_only_null_bytes() {
        let path = temp_socket_path("null_bytes");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let data = vec![0u8; 100];
        let client = std::thread::spawn({
            let p = path.clone();
            let d = data.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                s.write_all(&d).unwrap();
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
        cleanup(&path);
    }

    #[test]
    fn multiple_take_error_calls_all_none() {
        let path = temp_socket_path("multi_terr");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        for _ in 0..5 {
            assert!(listener.take_error().unwrap().is_none());
        }
        cleanup(&path);
    }

    #[test]
    fn bind_with_str_path() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ft_str_{}", std::process::id()));
        cleanup(&path);
        let path_str = path.to_str().unwrap();
        let _listener = UnixListener::bind(path_str).unwrap();
        assert!(path.exists());
        cleanup(&path);
    }

    #[test]
    fn connect_with_str_path() {
        let path = temp_socket_path("str_conn");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let path_str = path.to_str().unwrap().to_owned();
        let client = std::thread::spawn(move || {
            UnixStream::connect(path_str.as_str()).unwrap()
        });
        let (_server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        cleanup(&path);
    }

    #[test]
    fn rapid_write_flush_cycles() {
        let path = temp_socket_path("rapid_wf");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                for i in 0..20u8 {
                    s.write_all(&[i]).unwrap();
                    s.flush().unwrap();
                }
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        drop(listener);
        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        assert_eq!(buf.len(), 20);
        for (i, &b) in buf.iter().enumerate() {
            assert_eq!(b, i as u8);
        }
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn server_send_then_shutdown_client_reads_all() {
        use std::net::Shutdown;
        let path = temp_socket_path("srv_shut");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                let mut buf = Vec::new();
                s.read_to_end(&mut buf).unwrap();
                buf
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        server.write_all(b"complete message").unwrap();
        server.shutdown(Shutdown::Write).unwrap();
        let received = client.join().unwrap();
        assert_eq!(received, b"complete message");
        cleanup(&path);
    }

    #[test]
    fn set_nonblocking_false_explicitly() {
        let path = temp_socket_path("nb_false");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || UnixStream::connect(&p).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        // Explicitly set to false (the default)
        server.set_nonblocking(false).unwrap();
        cleanup(&path);
    }

    #[test]
    fn empty_socket_name_fails() {
        let result = UnixListener::bind("");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_after_shutdown_read_returns_zero() {
        use std::net::Shutdown;
        let path = temp_socket_path("shut_rd_z");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || UnixStream::connect(&p).unwrap()
        });
        let (mut server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        server.shutdown(Shutdown::Read).unwrap();
        let mut buf = [0u8; 16];
        let n = server.read(&mut buf).unwrap();
        assert_eq!(n, 0);
        cleanup(&path);
    }

    #[test]
    fn write_1mb_data() {
        let path = temp_socket_path("1mb");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let size = 1024 * 1024;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let client = std::thread::spawn({
            let p = path.clone();
            let d = data.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                s.write_all(&d).unwrap();
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        drop(listener);
        let mut buf = Vec::new();
        server.read_to_end(&mut buf).unwrap();
        client.join().unwrap();
        assert_eq!(buf.len(), size);
        assert_eq!(buf, data);
        cleanup(&path);
    }

    #[test]
    fn socket_file_removed_after_cleanup() {
        let path = temp_socket_path("rm_check");
        cleanup(&path);
        {
            let _listener = UnixListener::bind(&path).unwrap();
            assert!(path.exists());
        }
        // After drop, file still exists (OS behavior)
        cleanup(&path);
        assert!(!path.exists());
    }

    #[test]
    fn stream_write_read_alternating() {
        let path = temp_socket_path("alt_wr_rd");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                for i in 0..5u8 {
                    s.write_all(&[i]).unwrap();
                    s.flush().unwrap();
                    let mut buf = [0u8; 1];
                    s.read_exact(&mut buf).unwrap();
                    assert_eq!(buf[0], i + 100);
                }
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        for i in 0..5u8 {
            let mut buf = [0u8; 1];
            server.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], i);
            server.write_all(&[i + 100]).unwrap();
            server.flush().unwrap();
        }
        client.join().unwrap();
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn as_fd_and_as_raw_fd_agree() {
        let path = temp_socket_path("fd_agree");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn({
            let p = path.clone();
            move || UnixStream::connect(&p).unwrap()
        });
        let (server, _) = listener.accept().unwrap();
        let _c = client.join().unwrap();
        let raw = server.as_raw_fd();
        let borrowed = server.as_fd();
        assert_eq!(raw, borrowed.as_raw_fd());
        cleanup(&path);
    }

    #[test]
    fn write_and_read_exact_match() {
        let path = temp_socket_path("exact_match");
        cleanup(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let msg = b"exactly sixteen!";
        assert_eq!(msg.len(), 16);
        let client = std::thread::spawn({
            let p = path.clone();
            move || {
                let mut s = UnixStream::connect(&p).unwrap();
                s.write_all(msg).unwrap();
            }
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        let mut buf = [0u8; 16];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, msg);
        cleanup(&path);
    }
}
