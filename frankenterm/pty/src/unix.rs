//! Working with pseudo-terminals

use crate::{
    Child, CommandBuilder, MasterPty, PollablePtyReader, PtyPair, PtySize, PtySystem, SlavePty,
};
use anyhow::{Error, bail};
use filedescriptor::FileDescriptor;
use libc::{self, winsize};
use nix::unistd::{PathconfVar, fpathconf};
use std::cell::RefCell;
use std::convert::TryInto as _;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::{io, mem, ptr};

pub use std::os::unix::io::RawFd;

const MAX_TTY_NAME_BUFFER_LEN: usize = 64 * 1024;

fn next_tty_name_buffer_len(current_len: usize) -> Option<usize> {
    if current_len > MAX_TTY_NAME_BUFFER_LEN {
        return None;
    }
    current_len
        .checked_mul(2)
        .filter(|next_len| *next_len > current_len)
}

#[derive(Default)]
pub struct UnixPtySystem {}

fn openpty(size: PtySize) -> anyhow::Result<(UnixMasterPty, UnixSlavePty)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;

    let mut size = winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    };

    let result = unsafe {
        // BSDish systems may require mut pointers to some args
        #[allow(clippy::unnecessary_mut_passed)]
        libc::openpty(
            &mut master,
            &mut slave,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut size,
        )
    };

    if result != 0 {
        bail!("failed to openpty: {:?}", io::Error::last_os_error());
    }

    let tty_name = tty_name(slave);

    let master = UnixMasterPty {
        fd: PtyFd(unsafe { FileDescriptor::from_raw_fd(master) }),
        took_writer: RefCell::new(false),
        writer_authority: Arc::new(UnixWriterAuthority::default()),
        tty_name,
    };
    let slave = UnixSlavePty {
        fd: PtyFd(unsafe { FileDescriptor::from_raw_fd(slave) }),
    };

    // Ensure that these descriptors will get closed when we execute
    // the child process.  This is done after constructing the Pty
    // instances so that we ensure that the Ptys get drop()'d if
    // the cloexec() functions fail (unlikely!).
    cloexec(master.fd.as_raw_fd())?;
    cloexec(slave.fd.as_raw_fd())?;

    Ok((master, slave))
}

impl PtySystem for UnixPtySystem {
    fn openpty(&self, size: PtySize) -> anyhow::Result<PtyPair> {
        let (master, slave) = openpty(size)?;
        Ok(PtyPair {
            master: Box::new(master),
            slave: Box::new(slave),
        })
    }
}

struct PtyFd(pub FileDescriptor);
impl std::ops::Deref for PtyFd {
    type Target = FileDescriptor;
    fn deref(&self) -> &FileDescriptor {
        &self.0
    }
}
impl std::ops::DerefMut for PtyFd {
    fn deref_mut(&mut self) -> &mut FileDescriptor {
        &mut self.0
    }
}

impl AsFd for PtyFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Read for PtyFd {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        match self.0.read(buf) {
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => {
                // EIO indicates that the slave pty has been closed.
                // Treat this as EOF so that std::io::Read::read_to_string
                // and similar functions gracefully terminate when they
                // encounter this condition
                Ok(0)
            }
            x => x,
        }
    }
}

fn tty_name(fd: RawFd) -> Option<PathBuf> {
    let mut buf = vec![0 as std::ffi::c_char; 128];

    loop {
        let res = unsafe { libc::ttyname_r(fd, buf.as_mut_ptr(), buf.len()) };

        if res == libc::ERANGE {
            let Some(next_len) = next_tty_name_buffer_len(buf.len()) else {
                // on macOS, if the buf is "too big", ttyname_r can
                // return ERANGE, even though that is supposed to
                // indicate buf is "too small".
                return None;
            };
            buf.resize(next_len, 0 as std::ffi::c_char);
            continue;
        }

        return if res == 0 {
            let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
            let osstr = OsStr::from_bytes(cstr.to_bytes());
            Some(PathBuf::from(osstr))
        } else {
            None
        };
    }
}

/// On Big Sur, Cocoa leaks various file descriptors to child processes,
/// so we need to make a pass through the open descriptors beyond just the
/// stdio descriptors and close them all out.
/// This is approximately equivalent to the darwin `posix_spawnattr_setflags`
/// option POSIX_SPAWN_CLOEXEC_DEFAULT which is used as a bit of a cheat
/// on macOS.
/// On Linux, gnome/mutter leak shell extension fds to wezterm too, so we
/// also need to make an effort to clean up the mess.
///
/// This function enumerates the open filedescriptors in the current process
/// and then will forcibly call close(2) on each open fd that is numbered
/// 3 or higher, effectively closing all descriptors except for the stdio
/// streams.
///
/// The implementation of this function relies on `/dev/fd` being available
/// to provide the list of open fds.  Any errors in enumerating or closing
/// the fds are silently ignored.
pub fn close_random_fds() {
    // FreeBSD, macOS and presumably other BSDish systems have /dev/fd as
    // a directory listing the current fd numbers for the process.
    //
    // On Linux, /dev/fd is a symlink to /proc/self/fd
    if let Ok(dir) = std::fs::read_dir("/dev/fd") {
        let mut fds = vec![];
        for entry in dir {
            if let Some(num) = entry
                .ok()
                .map(|e| e.file_name())
                .and_then(|s| s.into_string().ok())
                .and_then(|n| n.parse::<libc::c_int>().ok())
            {
                if num > 2 {
                    fds.push(num);
                }
            }
        }
        for fd in fds {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

impl PtyFd {
    fn resize(&self, size: PtySize) -> Result<(), Error> {
        let ws_size = winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };

        if unsafe {
            libc::ioctl(
                self.0.as_raw_fd(),
                libc::TIOCSWINSZ as _,
                &ws_size as *const _,
            )
        } != 0
        {
            bail!(
                "failed to ioctl(TIOCSWINSZ): {:?}",
                io::Error::last_os_error()
            );
        }

        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, Error> {
        let mut size: winsize = unsafe { mem::zeroed() };
        if unsafe {
            libc::ioctl(
                self.0.as_raw_fd(),
                libc::TIOCGWINSZ as _,
                &mut size as *mut _,
            )
        } != 0
        {
            bail!(
                "failed to ioctl(TIOCGWINSZ): {:?}",
                io::Error::last_os_error()
            );
        }
        Ok(PtySize {
            rows: size.ws_row,
            cols: size.ws_col,
            pixel_width: size.ws_xpixel,
            pixel_height: size.ws_ypixel,
        })
    }

    fn spawn_command(&self, builder: CommandBuilder) -> anyhow::Result<std::process::Child> {
        let configured_umask = builder.umask;

        let mut cmd = builder.as_command()?;
        let controlling_tty = builder.get_controlling_tty();

        unsafe {
            cmd.stdin(self.as_stdio()?)
                .stdout(self.as_stdio()?)
                .stderr(self.as_stdio()?)
                .pre_exec(move || {
                    // Clean up a few things before we exec the program
                    // Clear out any potentially problematic signal
                    // dispositions that we might have inherited
                    for signo in &[
                        libc::SIGCHLD,
                        libc::SIGHUP,
                        libc::SIGINT,
                        libc::SIGQUIT,
                        libc::SIGTERM,
                        libc::SIGALRM,
                    ] {
                        libc::signal(*signo, libc::SIG_DFL);
                    }

                    let empty_set: libc::sigset_t = std::mem::zeroed();
                    libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());

                    // Establish ourselves as a session leader.
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }

                    // Clippy wants us to explicitly cast TIOCSCTTY using
                    // type::from(), but the size and potentially signedness
                    // are system dependent, which is why we're using `as _`.
                    // Suppress this lint for this section of code.
                    #[allow(clippy::cast_lossless)]
                    if controlling_tty {
                        // Set the pty as the controlling terminal.
                        // Failure to do this means that delivery of
                        // SIGWINCH won't happen when we resize the
                        // terminal, among other undesirable effects.
                        if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                            return Err(io::Error::last_os_error());
                        }
                    }

                    close_random_fds();

                    if let Some(mask) = configured_umask {
                        libc::umask(mask);
                    }

                    Ok(())
                })
        };

        let mut child = cmd.spawn()?;

        // Ensure that we close out the slave fds that Child retains;
        // they are not what we need (we need the master side to reference
        // them) and won't work in the usual way anyway.
        // In practice these are None, but it seems best to be move them
        // out in case the behavior of Command changes in the future.
        child.stdin.take();
        child.stdout.take();
        child.stderr.take();

        Ok(child)
    }
}

/// Represents the master end of a pty.
/// The file descriptor will be closed when the Pty is dropped.
struct UnixMasterPty {
    fd: PtyFd,
    took_writer: RefCell<bool>,
    writer_authority: Arc<UnixWriterAuthority>,
    tty_name: Option<PathBuf>,
}

struct UnixWriterAuthority {
    state: AtomicU8,
}

const WRITER_AUTHORITY_OPEN: u8 = 0;
const WRITER_AUTHORITY_ACTIVE: u8 = 1;
const WRITER_AUTHORITY_EOF_IN_PROGRESS: u8 = 2;
const WRITER_AUTHORITY_EOF_SENT: u8 = 3;
const WRITER_AUTHORITY_EOF_INDETERMINATE: u8 = 4;

impl Default for UnixWriterAuthority {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(WRITER_AUTHORITY_OPEN),
        }
    }
}

impl UnixWriterAuthority {
    fn claim(self: &Arc<Self>) -> Result<UnixWriterClaim, Error> {
        self.state
            .compare_exchange(
                WRITER_AUTHORITY_OPEN,
                WRITER_AUTHORITY_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(writer_authority_error)?;
        Ok(UnixWriterClaim {
            authority: Arc::clone(self),
            active: true,
        })
    }

    fn begin_terminal_eof(self: &Arc<Self>) -> Result<UnixTerminalEofAttempt, Error> {
        self.state
            .compare_exchange(
                WRITER_AUTHORITY_OPEN,
                WRITER_AUTHORITY_EOF_IN_PROGRESS,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(writer_authority_error)?;
        Ok(UnixTerminalEofAttempt {
            authority: Arc::clone(self),
            disposition: UnixTerminalEofDisposition::Reopen,
        })
    }
}

struct UnixWriterClaim {
    authority: Arc<UnixWriterAuthority>,
    active: bool,
}

impl Drop for UnixWriterClaim {
    fn drop(&mut self) {
        if self.active {
            let _ = self.authority.state.compare_exchange(
                WRITER_AUTHORITY_ACTIVE,
                WRITER_AUTHORITY_OPEN,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

#[derive(Clone, Copy)]
enum UnixTerminalEofDisposition {
    Reopen,
    Sent,
    Indeterminate,
}

struct UnixTerminalEofAttempt {
    authority: Arc<UnixWriterAuthority>,
    disposition: UnixTerminalEofDisposition,
}

impl UnixTerminalEofAttempt {
    fn commit_sent(mut self) {
        self.disposition = UnixTerminalEofDisposition::Sent;
    }

    fn commit_indeterminate(mut self) {
        self.disposition = UnixTerminalEofDisposition::Indeterminate;
    }
}

impl Drop for UnixTerminalEofAttempt {
    fn drop(&mut self) {
        let state = match self.disposition {
            UnixTerminalEofDisposition::Reopen => WRITER_AUTHORITY_OPEN,
            UnixTerminalEofDisposition::Sent => WRITER_AUTHORITY_EOF_SENT,
            UnixTerminalEofDisposition::Indeterminate => WRITER_AUTHORITY_EOF_INDETERMINATE,
        };
        self.authority.state.store(state, Ordering::Release);
    }
}

fn writer_authority_error(state: u8) -> Error {
    match state {
        WRITER_AUTHORITY_EOF_SENT => anyhow::anyhow!("terminal EOF has already been sent"),
        WRITER_AUTHORITY_EOF_INDETERMINATE => {
            anyhow::anyhow!("terminal EOF effect is indeterminate")
        }
        _ => anyhow::anyhow!("PTY writer authority is already active"),
    }
}

/// Represents the slave end of a pty.
/// The file descriptor will be closed when the Pty is dropped.
struct UnixSlavePty {
    fd: PtyFd,
}

/// Helper function to set the close-on-exec flag for a raw descriptor
fn cloexec(fd: RawFd) -> Result<(), Error> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        bail!(
            "fcntl to read flags failed: {:?}",
            io::Error::last_os_error()
        );
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if result == -1 {
        bail!(
            "fcntl to set CLOEXEC failed: {:?}",
            io::Error::last_os_error()
        );
    }
    Ok(())
}

impl SlavePty for UnixSlavePty {
    fn spawn_command(
        &self,
        builder: CommandBuilder,
    ) -> Result<Box<dyn Child + Send + Sync>, Error> {
        Ok(Box::new(self.fd.spawn_command(builder)?))
    }
}

impl MasterPty for UnixMasterPty {
    fn resize(&self, size: PtySize) -> Result<(), Error> {
        self.fd.resize(size)
    }

    fn get_size(&self) -> Result<PtySize, Error> {
        self.fd.get_size()
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, Error> {
        let fd = PtyFd(self.fd.try_clone()?);
        Ok(Box::new(fd))
    }

    fn try_clone_pollable_reader(&self) -> Result<Box<dyn PollablePtyReader>, Error> {
        let mut fd = PtyFd(self.fd.try_clone()?);
        fd.set_non_blocking(true)?;
        Ok(Box::new(fd))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, Error> {
        if *self.took_writer.borrow() {
            anyhow::bail!("cannot take writer more than once");
        }
        let claim = self.writer_authority.claim()?;
        let fd = PtyFd(self.fd.try_clone()?);
        *self.took_writer.borrow_mut() = true;
        Ok(Box::new(UnixMasterWriter { fd, _claim: claim }))
    }

    fn take_writer_for_broker_proxy(&self) -> Result<Box<dyn Write + Send>, Error> {
        if *self.took_writer.borrow() {
            anyhow::bail!("cannot take writer more than once");
        }
        let claim = self.writer_authority.claim()?;
        let fd = PtyFd(self.fd.try_clone()?);
        *self.took_writer.borrow_mut() = true;
        Ok(Box::new(UnixBrokerProxyWriter { fd, _claim: claim }))
    }

    fn send_terminal_eof(&self) -> Result<(), Error> {
        let mut fd = PtyFd(self.fd.try_clone()?);
        let bytes = prepare_terminal_eof(&fd)?;
        let attempt = self.writer_authority.begin_terminal_eof()?;
        deliver_terminal_eof(&mut fd.0, bytes, attempt)
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.fd.0.as_raw_fd())
    }

    fn tty_name(&self) -> Option<PathBuf> {
        self.tty_name.clone()
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        match unsafe { libc::tcgetpgrp(self.fd.0.as_raw_fd()) } {
            pid if pid > 0 => Some(pid),
            _ => None,
        }
    }

    fn get_termios(&self) -> Option<nix::sys::termios::Termios> {
        nix::sys::termios::tcgetattr(self.fd.0.as_fd()).ok()
    }
}

/// Represents the master end of a pty.
/// EOT will be sent, and then the file descriptor will be closed when
/// the Pty is dropped.
struct UnixMasterWriter {
    fd: PtyFd,
    _claim: UnixWriterClaim,
}

impl Drop for UnixMasterWriter {
    fn drop(&mut self) {
        let mut t: libc::termios = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
        if unsafe { libc::tcgetattr(self.fd.0.as_raw_fd(), &mut t) } == 0 {
            // EOF is only interpreted after a newline, so if it is set,
            // we send a newline followed by EOF.
            let eot = t.c_cc[libc::VEOF];
            if eot != 0 {
                let _ = self.fd.0.write_all(&[b'\n', eot]);
            }
        }
    }
}

impl Write for UnixMasterWriter {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        self.fd.write(buf)
    }
    fn flush(&mut self) -> Result<(), io::Error> {
        self.fd.flush()
    }
}

/// Broker-retained proxy writer. Its destructor is intentionally byte-silent;
/// closing the broker proxy must not become terminal input.
struct UnixBrokerProxyWriter {
    fd: PtyFd,
    _claim: UnixWriterClaim,
}

impl Write for UnixBrokerProxyWriter {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        self.fd.write(buf)
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        self.fd.flush()
    }
}

fn prepare_terminal_eof(fd: &PtyFd) -> Result<[u8; 2], io::Error> {
    let mut termios: libc::termios = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    if unsafe { libc::tcgetattr(fd.0.as_raw_fd(), &mut termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let disabled = fpathconf(fd.as_fd(), PathconfVar::_POSIX_VDISABLE)
        .map_err(io::Error::from)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "VEOF disable value unknown"))?;
    let disabled: libc::cc_t = disabled.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "VEOF disable value is outside cc_t range",
        )
    })?;
    let eof = terminal_eof_byte(&termios, disabled)?;
    // EOF is interpreted only after a line boundary in canonical mode.
    Ok([b'\n', eof])
}

fn deliver_terminal_eof<W: Write>(
    writer: &mut W,
    bytes: [u8; 2],
    attempt: UnixTerminalEofAttempt,
) -> Result<(), io::Error> {
    match writer.write(&bytes) {
        Ok(2) => {
            attempt.commit_sent();
            Ok(())
        }
        Ok(_) => {
            attempt.commit_indeterminate();
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "terminal EOF sequence was only partially accepted",
            ))
        }
        Err(error) => {
            attempt.commit_indeterminate();
            Err(error)
        }
    }
}

fn terminal_eof_byte(termios: &libc::termios, disabled: libc::cc_t) -> Result<u8, io::Error> {
    if termios.c_lflag & libc::ICANON == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminal EOF byte has no EOF semantics outside canonical mode",
        ));
    }
    let eof = termios.c_cc[libc::VEOF];
    if eof == disabled {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminal VEOF control character is disabled",
        ));
    }
    Ok(eof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::io::Read as _;
    use std::thread;
    use std::time::{Duration, Instant};

    fn read_until(reader: &mut dyn PollablePtyReader, needle: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        let mut chunk = [0_u8; 256];
        while Instant::now() < deadline {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    output.extend_from_slice(&chunk[..count]);
                    if output.windows(needle.len()).any(|window| window == needle) {
                        return output;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("PTY read failed: {error}"),
            }
        }
        panic!(
            "timed out waiting for {:?}; output was {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&output)
        );
    }

    #[test]
    fn tty_name_buffer_growth_is_bounded() {
        assert_eq!(next_tty_name_buffer_len(128), Some(256));
        assert_eq!(
            next_tty_name_buffer_len(MAX_TTY_NAME_BUFFER_LEN),
            Some(MAX_TTY_NAME_BUFFER_LEN * 2)
        );
        assert_eq!(next_tty_name_buffer_len(MAX_TTY_NAME_BUFFER_LEN + 1), None);
        assert_eq!(next_tty_name_buffer_len(usize::MAX), None);
        assert_eq!(next_tty_name_buffer_len(0), None);
    }

    #[test]
    fn broker_proxy_writer_drop_is_silent_and_terminal_eof_is_explicit_once() {
        let (broker_master, slave) = openpty(PtySize::default()).expect("open native test PTY");
        let mut broker_reader = broker_master
            .try_clone_pollable_reader()
            .expect("broker proxy reader");
        let mut broker_writer = broker_master
            .take_writer_for_broker_proxy()
            .expect("broker proxy writer");

        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg(concat!(
            "IFS= read -r first; printf 'first:%s\\n' \"$first\"; ",
            "IFS= read -r second; printf 'second:%s\\n' \"$second\"; ",
            "IFS= read -r third; printf 'third:%s\\n' \"$third\""
        ));
        let mut child = slave.spawn_command(command).expect("spawn retained child");
        drop(slave);

        broker_writer.write_all(b"alpha\n").expect("write alpha");
        broker_writer.flush().expect("flush alpha");
        let _ = read_until(broker_reader.as_mut(), b"first:alpha");

        // A logical guardian rotation does not touch broker-owned I/O.
        broker_writer.write_all(b"beta\n").expect("write beta");
        broker_writer.flush().expect("flush beta");
        let _ = read_until(broker_reader.as_mut(), b"second:beta");
        drop(broker_writer);

        thread::sleep(Duration::from_millis(100));
        assert!(
            child
                .try_wait()
                .expect("poll after byte-silent proxy close")
                .is_none(),
            "broker proxy writer drop injected terminal input or EOF"
        );

        assert!(broker_master.send_terminal_eof().is_ok());
        assert!(
            broker_master.send_terminal_eof().is_err(),
            "terminal EOF authority was reusable"
        );
        assert!(child.wait().expect("wait after explicit EOF").success());
    }

    #[test]
    fn ordinary_writer_drop_preserves_legacy_newline_and_terminal_eof() {
        let (master, slave) = openpty(PtySize::default()).expect("open native test PTY");
        let mut reader = master
            .try_clone_pollable_reader()
            .expect("pollable test reader");
        let mut writer = master.take_writer().expect("ordinary PTY writer");
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg(concat!(
            "IFS= read -r first; printf 'first:%s\\n' \"$first\"; ",
            "IFS= read -r drop_line; printf 'drop-line:%s\\n' \"$drop_line\"; ",
            "if IFS= read -r final; then printf 'unexpected:%s\\n' \"$final\"; ",
            "else printf 'drop-eof\\n'; fi"
        ));
        let mut child = slave.spawn_command(command).expect("spawn test child");
        drop(slave);

        writer.write_all(b"alpha\n").expect("write alpha");
        let _ = read_until(reader.as_mut(), b"first:alpha");
        drop(writer);

        let output = read_until(reader.as_mut(), b"drop-eof");
        assert!(
            output
                .windows(b"drop-line:".len())
                .any(|window| window == b"drop-line:")
        );
        assert!(child.wait().expect("wait after writer drop").success());
    }

    #[test]
    fn ordinary_writer_drop_keeps_historical_raw_mode_byte_delivery() {
        let (master, slave) = openpty(PtySize::default()).expect("open native test PTY");
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.fd.0.as_raw_fd(), &mut termios) },
            0
        );
        unsafe { libc::cfmakeraw(&mut termios) };
        let eot = termios.c_cc[libc::VEOF];
        assert_ne!(eot, 0, "test terminal unexpectedly disables VEOF");
        assert_eq!(
            unsafe { libc::tcsetattr(slave.fd.0.as_raw_fd(), libc::TCSANOW, &termios) },
            0
        );

        let mut reader = master
            .try_clone_pollable_reader()
            .expect("pollable raw-mode reader");
        let writer = master.take_writer().expect("ordinary raw-mode writer");
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg("bytes=$(od -An -t u1 -N 2); printf 'bytes:%s:end\\n' \"$bytes\"");
        let mut child = slave.spawn_command(command).expect("spawn raw-mode reader");
        drop(slave);

        drop(writer);
        let output = read_until(reader.as_mut(), b":end");
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("bytes:"));
        assert!(
            output.contains("10"),
            "legacy newline byte missing: {output:?}"
        );
        assert!(
            output.contains(&eot.to_string()),
            "legacy VEOF byte missing: {output:?}"
        );
        assert!(child.wait().expect("wait for raw-mode reader").success());
    }

    #[test]
    fn failed_terminal_eof_attempt_reopens_authority_for_retry() {
        let authority = Arc::new(UnixWriterAuthority::default());
        let attempt = authority
            .begin_terminal_eof()
            .expect("begin first EOF attempt");
        drop(attempt);
        let claim = authority.claim().expect("failed attempt must reopen state");
        drop(claim);

        let attempt = authority
            .begin_terminal_eof()
            .expect("begin successful EOF attempt");
        attempt.commit_sent();
        assert!(
            authority.claim().is_err(),
            "committed terminal EOF allowed another writer"
        );
    }

    struct ShortTerminalEofWriter(usize);

    impl Write for ShortTerminalEofWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(self.0.min(bytes.len()))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_or_zero_terminal_eof_write_is_indeterminate_and_not_retried() {
        for accepted in [0, 1] {
            let authority = Arc::new(UnixWriterAuthority::default());
            let attempt = authority
                .begin_terminal_eof()
                .expect("begin terminal EOF attempt");
            let error =
                deliver_terminal_eof(&mut ShortTerminalEofWriter(accepted), [b'\n', 4], attempt)
                    .expect_err("short terminal EOF write must fail closed");
            assert_eq!(error.kind(), ErrorKind::WriteZero);
            assert_eq!(
                authority.state.load(Ordering::Acquire),
                WRITER_AUTHORITY_EOF_INDETERMINATE
            );
            assert!(authority.begin_terminal_eof().is_err());
            assert!(authority.claim().is_err());
        }
    }

    #[test]
    fn disabled_veof_and_noncanonical_mode_reject_terminal_eof_semantics() {
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        termios.c_lflag = libc::ICANON;
        termios.c_cc[libc::VEOF] = 0;
        assert_eq!(
            terminal_eof_byte(&termios, 0)
                .expect_err("disabled VEOF must fail")
                .kind(),
            ErrorKind::Unsupported
        );

        termios.c_cc[libc::VEOF] = 4;
        termios.c_lflag &= !libc::ICANON;
        assert_eq!(
            terminal_eof_byte(&termios, 0)
                .expect_err("noncanonical VEOF must fail")
                .kind(),
            ErrorKind::Unsupported
        );
    }
}
