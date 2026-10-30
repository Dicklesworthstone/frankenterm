//! A binary-clean, same-account transport to an already running mux.
//!
//! The SSH account is the access boundary. The existing client and server
//! retain codec negotiation, frame limits and exact-pane authority; this
//! relay neither invents a second protocol nor grants a narrower device scope.

use anyhow::{Context, bail, ensure};
use filedescriptor::{POLLIN, POLLOUT, pollfd};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::sys::socket::{
    AddressFamily, Shutdown, SockFlag, SockType, UnixAddr, connect, getsockopt, shutdown, socket,
    sockopt,
};
use nix::unistd::{dup, geteuid, read, write};
use std::ffi::OsString;
use std::fs::Metadata;
use std::io::IsTerminal;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const BUFFER_BYTES: usize = 32 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn run(explicit_socket: Option<&Path>) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    ensure!(
        !stdin.is_terminal() && !stdout.is_terminal(),
        "binary mux traffic requires piped stdin and stdout, without an SSH PTY"
    );
    let path = select_socket(
        explicit_socket,
        std::env::var_os("FRANKENTERM_UNIX_SOCKET"),
        || config::RUNTIME_DIR.join("sock"),
    )?;
    let server = connect_existing(&path, CONNECT_TIMEOUT)?;
    let input = NonblockingDescriptor::duplicate(&stdin)?;
    let output = NonblockingDescriptor::duplicate(&stdout)?;
    relay(&input.fd, &output.fd, &server, STALL_TIMEOUT, DRAIN_TIMEOUT)
}

fn select_socket(
    explicit: Option<&Path>,
    environment: Option<OsString>,
    default: impl FnOnce() -> PathBuf,
) -> anyhow::Result<PathBuf> {
    let path = explicit
        .map(Path::to_path_buf)
        .or_else(|| environment.map(PathBuf::from))
        .unwrap_or_else(default);
    ensure!(
        path.is_absolute() && path.file_name().is_some(),
        "the mux socket must be an absolute file path; use --socket to select it"
    );
    // Validates the actual platform's sockaddr_un capacity before filesystem
    // or network access. No shell expansion, executable config or discovery.
    UnixAddr::new(&path).context("the selected mux socket path is invalid or too long")?;
    Ok(path)
}

fn inspect_socket(path: &Path) -> anyhow::Result<Metadata> {
    let metadata = std::fs::symlink_metadata(path).context(
        "the selected mux socket is unavailable; start FrankenTerm separately or choose --socket",
    )?;
    ensure!(
        metadata.file_type().is_socket() && metadata.nlink() == 1,
        "the selected endpoint must be a socket, not a file or symbolic link"
    );
    ensure!(
        metadata.uid() == geteuid().as_raw() && metadata.mode() & 0o022 == 0,
        "the mux socket must belong to this account and forbid group/other writes"
    );
    Ok(metadata)
}

fn same_socket(before: &Metadata, after: &Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
        && before.uid() == after.uid()
        && before.mode() == after.mode()
}

fn peer_uid(fd: &OwnedFd) -> anyhow::Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        Ok(getsockopt(fd, sockopt::PeerCredentials)?.uid())
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        Ok(nix::unistd::getpeereid(fd)?.0.as_raw())
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    {
        let _ = fd;
        bail!("this platform cannot verify the local mux account")
    }
}

fn connect_existing(path: &Path, timeout: Duration) -> anyhow::Result<OwnedFd> {
    let before = inspect_socket(path)?;
    let address = UnixAddr::new(path)?;
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;
    // Apple has no SOCK_NONBLOCK/SOCK_CLOEXEC socket flags. This command runs
    // before worker creation and starts no subprocess, so set both explicitly.
    fcntl(&fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    match connect(fd.as_raw_fd(), &address) {
        Ok(()) => {}
        Err(Errno::EINPROGRESS) => {
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(
                    !remaining.is_zero(),
                    "connecting to the existing mux timed out"
                );
                let mut interest = [pollfd {
                    fd: fd.as_raw_fd(),
                    events: POLLOUT,
                    revents: 0,
                }];
                if wait(&mut interest, remaining)? && interest[0].revents != 0 {
                    break;
                }
            }
            let error = getsockopt(&fd, sockopt::SocketError)?;
            ensure!(
                error == 0,
                "the existing mux rejected the connection: {}",
                Errno::from_raw(error)
            );
        }
        // A full Unix listener backlog can return EAGAIN. It is a refusal,
        // not permission to start a new server or spin retrying the endpoint.
        Err(error) => return Err(error).context("could not connect to the existing mux"),
    }
    ensure!(
        peer_uid(&fd)? == geteuid().as_raw(),
        "the connected mux belongs to another account"
    );
    let after = inspect_socket(path)?;
    ensure!(
        same_socket(&before, &after),
        "the selected mux socket changed while connecting; select it again"
    );
    Ok(fd)
}

/// dup shares file status flags. Retain and restore those flags so returning
/// from the relay cannot change an inherited pipe's other owner.
struct NonblockingDescriptor {
    fd: OwnedFd,
    original: OFlag,
}

impl NonblockingDescriptor {
    fn duplicate(source: &impl AsFd) -> anyhow::Result<Self> {
        let fd = dup(source)?;
        fcntl(&fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        let original = OFlag::from_bits_retain(fcntl(&fd, FcntlArg::F_GETFL)?);
        fcntl(&fd, FcntlArg::F_SETFL(original | OFlag::O_NONBLOCK))?;
        Ok(Self { fd, original })
    }
}

impl Drop for NonblockingDescriptor {
    fn drop(&mut self) {
        if let Err(error) = fcntl(&self.fd, FcntlArg::F_SETFL(self.original)) {
            eprintln!("ft mobile attach: could not restore inherited descriptor flags: {error}");
        }
    }
}

struct Buffer {
    bytes: [u8; BUFFER_BYTES],
    start: usize,
    end: usize,
    deadline: Option<Instant>,
}

impl Buffer {
    fn new() -> Self {
        Self {
            bytes: [0; BUFFER_BYTES],
            start: 0,
            end: 0,
            deadline: None,
        }
    }

    fn empty(&self) -> bool {
        self.start == self.end
    }

    /// Read only into an empty fixed buffer. Return true only for actual EOF.
    fn receive(&mut self, fd: &OwnedFd, stall: Duration) -> anyhow::Result<bool> {
        debug_assert!(self.empty());
        match read(fd, &mut self.bytes) {
            Ok(0) => Ok(true),
            Ok(count) => {
                self.start = 0;
                self.end = count;
                self.deadline = Some(Instant::now() + stall);
                Ok(false)
            }
            Err(Errno::EAGAIN | Errno::EINTR) => Ok(false),
            Err(error) => Err(error).context("reading mux stdio traffic"),
        }
    }

    fn send(&mut self, fd: &OwnedFd, stall: Duration) -> anyhow::Result<()> {
        match write(fd, &self.bytes[self.start..self.end]) {
            Ok(0) => bail!("mux stdio output stopped accepting bytes"),
            Ok(count) => {
                self.start += count;
                self.deadline = (!self.empty()).then(|| Instant::now() + stall);
                Ok(())
            }
            Err(Errno::EAGAIN | Errno::EINTR) => Ok(()),
            Err(error) => Err(error).context("forwarding mux stdio traffic"),
        }
    }
}

fn wait(interests: &mut [pollfd], timeout: Duration) -> anyhow::Result<bool> {
    match filedescriptor::poll(interests, Some(timeout)) {
        Ok(ready) => Ok(ready != 0),
        Err(filedescriptor::Error::Poll(error) | filedescriptor::Error::Io(error))
            if error.kind() == std::io::ErrorKind::Interrupted =>
        {
            Ok(false)
        }
        Err(error) => Err(error).context("waiting for mux stdio readiness"),
    }
}

fn interest(interests: &mut Vec<pollfd>, fd: &OwnedFd, events: i16) -> usize {
    let index = interests.len();
    interests.push(pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    });
    index
}

fn relay(
    input: &OwnedFd,
    output: &OwnedFd,
    server: &OwnedFd,
    stall: Duration,
    drain: Duration,
) -> anyhow::Result<()> {
    let mut to_server = Buffer::new();
    let mut to_output = Buffer::new();
    let mut input_eof = false;
    let mut server_eof = false;
    let mut write_closed = false;
    let mut forwarding_error = None;
    let mut closing_deadline = None;
    let mut interests = Vec::with_capacity(3);
    loop {
        if server_eof && to_output.empty() {
            ensure!(
                to_server.empty(),
                "the mux closed before all admitted stdin bytes were forwarded"
            );
            return match forwarding_error {
                Some(error) => Err(error),
                None => Ok(()),
            };
        }
        if input_eof && to_server.empty() && !write_closed {
            shutdown(server.as_raw_fd(), Shutdown::Write)
                .context("closing the mux input stream")?;
            write_closed = true;
        }
        let now = Instant::now();
        let mut timeout = Duration::from_secs(1);
        for deadline in [to_server.deadline, to_output.deadline, closing_deadline]
            .into_iter()
            .flatten()
        {
            ensure!(
                deadline > now,
                "mux stdio forwarding timed out while draining or waiting for write progress"
            );
            timeout = timeout.min(deadline.duration_since(now));
        }

        // A single owner services both directions. No stdin reader can remain
        // blocked after server EOF; no queue grows behind a stalled consumer.
        interests.clear();
        let input_index = (!input_eof && !server_eof && to_server.empty())
            .then(|| interest(&mut interests, input, POLLIN));
        let output_index = (!to_output.empty()).then(|| interest(&mut interests, output, POLLOUT));
        let events = if !server_eof && to_output.empty() {
            POLLIN
        } else {
            0
        } | if !server_eof && !to_server.empty() {
            POLLOUT
        } else {
            0
        };
        let server_index = (events != 0).then(|| interest(&mut interests, server, events));
        ensure!(
            !interests.is_empty(),
            "mux stdio relay has no remaining stream owner"
        );
        if !wait(&mut interests, timeout)? {
            continue;
        }

        if let Some(index) = output_index
            && interests[index].revents != 0
        {
            to_output.send(output, stall)?;
        }
        if let Some(index) = server_index {
            let ready = interests[index].revents;
            // A mux may stop accepting input while its final reply is still
            // queued. Preserve that reply even if an admitted input chunk can
            // no longer be written; report the uncertain input after draining.
            if ready != 0 && to_output.empty() && to_output.receive(server, stall)? {
                server_eof = true;
                closing_deadline.get_or_insert_with(|| Instant::now() + drain);
            }
            if ready != 0
                && !server_eof
                && !to_server.empty()
                && let Err(error) = to_server.send(server, stall)
            {
                forwarding_error = Some(error);
                to_server.start = to_server.end;
                to_server.deadline = None;
                input_eof = true;
                write_closed = true;
                closing_deadline.get_or_insert_with(|| Instant::now() + drain);
            }
        }
        if let Some(index) = input_index
            && !server_eof
            && !input_eof
            && interests[index].revents != 0
            && to_server.receive(input, stall)?
        {
            input_eof = true;
            closing_deadline.get_or_insert_with(|| Instant::now() + drain);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn explicit_socket_wins_and_no_candidate_is_guessed_or_expanded() {
        let chosen = select_socket(
            Some(Path::new("/explicit.sock")),
            Some("/env.sock".into()),
            || panic!("not used"),
        )
        .unwrap();
        assert_eq!(chosen, Path::new("/explicit.sock"));
        assert_eq!(
            select_socket(None, Some("/env.sock".into()), || panic!("not used")).unwrap(),
            Path::new("/env.sock")
        );
        assert_eq!(
            select_socket(None, None, || PathBuf::from("/runtime/sock")).unwrap(),
            Path::new("/runtime/sock")
        );
        for candidate in ["", "relative.sock", "~/mux.sock", "/"] {
            assert!(
                select_socket(None, Some(candidate.into()), || panic!(
                    "invalid selection must not fall back"
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn connection_rejects_missing_files_links_and_writable_sockets_without_creating_anything() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.sock");
        assert!(connect_existing(&missing, CONNECT_TIMEOUT).is_err());
        assert!(!missing.exists());
        let file = directory.path().join("file");
        std::fs::write(&file, b"unchanged").unwrap();
        assert!(connect_existing(&file, CONNECT_TIMEOUT).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"unchanged");
        let path = directory.path().join("server.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("alias.sock");
        symlink(&path, &link).unwrap();
        assert!(connect_existing(&link, CONNECT_TIMEOUT).is_err());
        assert!(connect_existing(&path, CONNECT_TIMEOUT).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o622)).unwrap();
        assert!(connect_existing(&path, CONNECT_TIMEOUT).is_err());
    }

    #[test]
    fn nonblocking_stdio_restores_the_original_open_file_description_flags() {
        let (first, _second) = UnixStream::pair().unwrap();
        let before = fcntl(&first, FcntlArg::F_GETFL).unwrap();
        {
            let _descriptor = NonblockingDescriptor::duplicate(&first).unwrap();
            assert!(
                OFlag::from_bits_retain(fcntl(&first, FcntlArg::F_GETFL).unwrap())
                    .contains(OFlag::O_NONBLOCK)
            );
        }
        assert_eq!(fcntl(&first, FcntlArg::F_GETFL).unwrap(), before);
    }

    #[test]
    fn server_eof_retires_relay_even_when_stdin_remains_open() {
        let (input, _held_input_writer) = UnixStream::pair().unwrap();
        let (output, mut reader) = UnixStream::pair().unwrap();
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"\0mux-final\xff").unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let input = NonblockingDescriptor::duplicate(&input).unwrap();
        let output = NonblockingDescriptor::duplicate(&output).unwrap();
        let server = NonblockingDescriptor::duplicate(&server).unwrap();
        relay(
            &input.fd,
            &output.fd,
            &server.fd,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut received = [0; 11];
        reader.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"\0mux-final\xff");
    }

    #[test]
    fn stdin_eof_half_closes_mux_and_drains_the_final_binary_response() {
        let (input, mut writer) = UnixStream::pair().unwrap();
        let (output, mut reader) = UnixStream::pair().unwrap();
        let (server, mut peer) = UnixStream::pair().unwrap();
        writer.write_all(b"request\0\xff").unwrap();
        writer.shutdown(std::net::Shutdown::Write).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let owner = std::thread::spawn(move || {
            let mut request = Vec::new();
            peer.read_to_end(&mut request).unwrap();
            assert_eq!(request, b"request\0\xff");
            peer.write_all(b"response\0\xff").unwrap();
        });
        let input = NonblockingDescriptor::duplicate(&input).unwrap();
        let output = NonblockingDescriptor::duplicate(&output).unwrap();
        let server = NonblockingDescriptor::duplicate(&server).unwrap();
        relay(
            &input.fd,
            &output.fd,
            &server.fd,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap();
        owner.join().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut received = [0; 10];
        reader.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"response\0\xff");
    }

    #[test]
    fn a_peer_that_ignores_half_close_cannot_hold_the_relay_forever() {
        let (input, writer) = UnixStream::pair().unwrap();
        writer.shutdown(std::net::Shutdown::Write).unwrap();
        let (output, _reader) = UnixStream::pair().unwrap();
        let (server, _peer) = UnixStream::pair().unwrap();
        let input = NonblockingDescriptor::duplicate(&input).unwrap();
        let output = NonblockingDescriptor::duplicate(&output).unwrap();
        let server = NonblockingDescriptor::duplicate(&server).unwrap();
        let started = Instant::now();
        let error = relay(
            &input.fd,
            &output.fd,
            &server.fd,
            Duration::from_secs(1),
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn large_binary_streams_cross_both_bounded_buffers_without_loss() {
        let (input, mut writer) = UnixStream::pair().unwrap();
        let (output, mut reader) = UnixStream::pair().unwrap();
        let (server, mut peer) = UnixStream::pair().unwrap();
        let payload: Vec<u8> = (0..BUFFER_BYTES * 17).map(|i| (i % 251) as u8).collect();
        for stream in [&writer, &reader, &peer] {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
        }
        let expected = payload.clone();
        let producer = std::thread::spawn(move || {
            writer.write_all(&payload).unwrap();
            writer.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let responder = std::thread::spawn(move || {
            let mut chunk = [0; 997];
            loop {
                let count = peer.read(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                peer.write_all(&chunk[..count]).unwrap();
            }
        });
        let consumer = std::thread::spawn(move || {
            let mut received = vec![0; expected.len()];
            reader.read_exact(&mut received).unwrap();
            assert_eq!(received, expected);
        });
        let input = NonblockingDescriptor::duplicate(&input).unwrap();
        let output = NonblockingDescriptor::duplicate(&output).unwrap();
        let server = NonblockingDescriptor::duplicate(&server).unwrap();
        relay(
            &input.fd,
            &output.fd,
            &server.fd,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();
        producer.join().unwrap();
        responder.join().unwrap();
        consumer.join().unwrap();
    }

    #[test]
    fn rejected_input_does_not_discard_the_muxs_final_reply() {
        let (input, mut writer) = UnixStream::pair().unwrap();
        let (output, mut reader) = UnixStream::pair().unwrap();
        let (server, mut peer) = UnixStream::pair().unwrap();
        writer.write_all(b"input-that-cannot-be-accepted").unwrap();
        peer.shutdown(std::net::Shutdown::Read).unwrap();
        let response = vec![0xa5; BUFFER_BYTES * 3];
        let expected = response.clone();
        peer.set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let responder = std::thread::spawn(move || {
            peer.write_all(&response).unwrap();
            peer.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let consumer = std::thread::spawn(move || {
            let mut received = vec![0; expected.len()];
            reader.read_exact(&mut received).unwrap();
            assert_eq!(received, expected);
        });
        let input = NonblockingDescriptor::duplicate(&input).unwrap();
        let output = NonblockingDescriptor::duplicate(&output).unwrap();
        let server = NonblockingDescriptor::duplicate(&server).unwrap();
        assert!(
            relay(
                &input.fd,
                &output.fd,
                &server.fd,
                Duration::from_secs(5),
                Duration::from_secs(5)
            )
            .is_err()
        );
        responder.join().unwrap();
        consumer.join().unwrap();
    }
}
