//! This crate provides a cross platform API for working with the
//! psuedo terminal (pty) interfaces provided by the system.
//! Unlike other crates in this space, this crate provides a set
//! of traits that allow selecting from different implementations
//! at runtime.
//! This crate is part of [wezterm](https://github.com/wezterm/wezterm).
//!
//! ```no_run
//! use portable_pty::{CommandBuilder, PtySize, native_pty_system, PtySystem};
//! use anyhow::Error;
//!
//! // Use the native pty implementation for the system
//! let pty_system = native_pty_system();
//!
//! // Create a new pty
//! let mut pair = pty_system.openpty(PtySize {
//!     rows: 24,
//!     cols: 80,
//!     // Not all systems support pixel_width, pixel_height,
//!     // but it is good practice to set it to something
//!     // that matches the size of the selected font.  That
//!     // is more complex than can be shown here in this
//!     // brief example though!
//!     pixel_width: 0,
//!     pixel_height: 0,
//! })?;
//!
//! // Spawn a shell into the pty
//! let cmd = CommandBuilder::new("bash");
//! let child = pair.slave.spawn_command(cmd)?;
//!
//! // Read and parse output from the pty with reader
//! let mut reader = pair.master.try_clone_reader()?;
//!
//! // Send data to the pty by writing to the master
//! writeln!(pair.master.take_writer()?, "ls -l\r\n")?;
//! # Ok::<(), Error>(())
//! ```
//!
use anyhow::Error;
use downcast_rs::{impl_downcast, Downcast};
#[cfg(unix)]
use libc;
#[cfg(feature = "serde_support")]
use serde::{Deserialize, Serialize};
use std::io::Result as IoResult;
#[cfg(windows)]
use std::os::windows::prelude::{AsRawHandle, RawHandle};

pub mod cmdbuilder;
pub use cmdbuilder::CommandBuilder;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod win;

pub mod serial;

/// Represents the size of the visible display area in the pty
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde_support", derive(Serialize, Deserialize))]
pub struct PtySize {
    /// The number of lines of text
    pub rows: u16,
    /// The number of columns of text
    pub cols: u16,
    /// The width of a cell in pixels.  Note that some systems never
    /// fill this value and ignore it.
    pub pixel_width: u16,
    /// The height of a cell in pixels.  Note that some systems never
    /// fill this value and ignore it.
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// Represents the master/control end of the pty
pub trait MasterPty: Downcast + Send {
    /// Inform the kernel and thus the child process that the window resized.
    /// It will update the winsize information maintained by the kernel,
    /// and generate a signal for the child to notice and update its state.
    fn resize(&self, size: PtySize) -> Result<(), Error>;
    /// Retrieves the size of the pty as known by the kernel
    fn get_size(&self) -> Result<PtySize, Error>;
    /// Obtain a readable handle; output from the slave(s) is readable
    /// via this stream.
    fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, Error>;
    /// Obtain a writable handle; writing to it will send data to the
    /// slave end.
    /// Dropping the writer will send EOF to the slave end.
    /// It is invalid to take the writer more than once.
    fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, Error>;

    /// If applicable to the type of the tty, return the local process id
    /// of the process group or session leader
    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<libc::pid_t>;

    /// If get_termios() and process_group_leader() are both implemented and
    /// return Some, then as_raw_fd() should return the same underlying fd
    /// associated with the stream. This is to enable applications that
    /// "know things" to query similar information for themselves.
    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<unix::RawFd>;

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf>;

    /// If applicable to the type of the tty, return the termios
    /// associated with the stream
    #[cfg(unix)]
    fn get_termios(&self) -> Option<nix::sys::termios::Termios> {
        None
    }
}
impl_downcast!(MasterPty);

/// Represents a child process spawned into the pty.
/// This handle can be used to wait for or terminate that child process.
pub trait Child: std::fmt::Debug + ChildKiller + Downcast + Send {
    /// Poll the child to see if it has completed.
    /// Does not block.
    /// Returns None if the child has not yet terminated,
    /// else returns its exit status.
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>>;
    /// Blocks execution until the child process has completed,
    /// yielding its exit status.
    fn wait(&mut self) -> IoResult<ExitStatus>;
    /// Returns the process identifier of the child process,
    /// if applicable
    fn process_id(&self) -> Option<u32>;
    /// Returns the process handle of the child process, if applicable.
    /// Only available on Windows.
    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle>;
}
impl_downcast!(Child);

/// Represents the ability to signal a Child to terminate
pub trait ChildKiller: std::fmt::Debug + Downcast + Send {
    /// Terminate the child process
    fn kill(&mut self) -> IoResult<()>;

    /// Clone an object that can be split out from the Child in order
    /// to send it signals independently from a thread that may be
    /// blocked in `.wait`.
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync>;
}
impl_downcast!(ChildKiller);

/// Represents the slave side of a pty.
/// Can be used to spawn processes into the pty.
pub trait SlavePty {
    /// Spawns the command specified by the provided CommandBuilder
    fn spawn_command(&self, cmd: CommandBuilder) -> Result<Box<dyn Child + Send + Sync>, Error>;
}

/// Represents the exit status of a child process.
#[derive(Debug, Clone)]
pub struct ExitStatus {
    code: u32,
    signal: Option<String>,
}

impl ExitStatus {
    /// Construct an ExitStatus from a process return code
    pub fn with_exit_code(code: u32) -> Self {
        Self { code, signal: None }
    }

    /// Construct an ExitStatus from a signal name
    pub fn with_signal(signal: &str) -> Self {
        Self {
            code: 1,
            signal: Some(signal.to_string()),
        }
    }

    /// Returns true if the status indicates successful completion
    pub fn success(&self) -> bool {
        match self.signal {
            None => self.code == 0,
            Some(_) => false,
        }
    }

    /// Returns the exit code that this ExitStatus was constructed with
    pub fn exit_code(&self) -> u32 {
        self.code
    }

    /// Returns the signal if present that this ExitStatus was constructed with
    pub fn signal(&self) -> Option<&str> {
        self.signal.as_deref()
    }
}

impl From<std::process::ExitStatus> for ExitStatus {
    fn from(status: std::process::ExitStatus) -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;

            if let Some(signal) = status.signal() {
                let signame = unsafe { libc::strsignal(signal) };
                let signal = if signame.is_null() {
                    format!("Signal {}", signal)
                } else {
                    let signame = unsafe { std::ffi::CStr::from_ptr(signame) };
                    signame.to_string_lossy().to_string()
                };

                return ExitStatus {
                    code: status.code().map(|c| c as u32).unwrap_or(1),
                    signal: Some(signal),
                };
            }
        }

        let code =
            status
                .code()
                .map(|c| c as u32)
                .unwrap_or_else(|| if status.success() { 0 } else { 1 });

        ExitStatus { code, signal: None }
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        if self.success() {
            write!(fmt, "Success")
        } else {
            match &self.signal {
                Some(sig) => write!(fmt, "Terminated by {}", sig),
                None => write!(fmt, "Exited with code {}", self.code),
            }
        }
    }
}

pub struct PtyPair {
    // slave is listed first so that it is dropped first.
    // The drop order is stable and specified by rust rfc 1857
    pub slave: Box<dyn SlavePty + Send>,
    pub master: Box<dyn MasterPty + Send>,
}

/// The `PtySystem` trait allows an application to work with multiple
/// possible Pty implementations at runtime.  This is important on
/// Windows systems which have a variety of implementations.
pub trait PtySystem: Downcast {
    /// Create a new Pty instance with the window size set to the specified
    /// dimensions.  Returns a (master, slave) Pty pair.  The master side
    /// is used to drive the slave side.
    fn openpty(&self, size: PtySize) -> anyhow::Result<PtyPair>;
}
impl_downcast!(PtySystem);

impl Child for std::process::Child {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        std::process::Child::try_wait(self).map(|s| match s {
            Some(s) => Some(s.into()),
            None => None,
        })
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        std::process::Child::wait(self).map(Into::into)
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.id())
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        Some(std::os::windows::io::AsRawHandle::as_raw_handle(self))
    }
}

#[derive(Debug)]
struct ProcessSignaller {
    pid: Option<u32>,

    #[cfg(windows)]
    handle: Option<filedescriptor::OwnedHandle>,
}

#[cfg(windows)]
impl ChildKiller for ProcessSignaller {
    fn kill(&mut self) -> IoResult<()> {
        if let Some(handle) = &self.handle {
            unsafe {
                if winapi::um::processthreadsapi::TerminateProcess(handle.as_raw_handle() as _, 127)
                    == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(Self {
            pid: self.pid,
            handle: self.handle.as_ref().and_then(|h| h.try_clone().ok()),
        })
    }
}

#[cfg(unix)]
impl ChildKiller for ProcessSignaller {
    fn kill(&mut self) -> IoResult<()> {
        if let Some(pid) = self.pid {
            let result = unsafe { libc::kill(pid as i32, libc::SIGHUP) };
            if result != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(Self { pid: self.pid })
    }
}

impl ChildKiller for std::process::Child {
    fn kill(&mut self) -> IoResult<()> {
        #[cfg(unix)]
        {
            // On unix, we send the SIGHUP signal instead of trying to kill
            // the process. The default behavior of a process receiving this
            // signal is to be killed unless it configured a signal handler.
            let result = unsafe { libc::kill(self.id() as i32, libc::SIGHUP) };
            if result != 0 {
                return Err(std::io::Error::last_os_error());
            }

            // We successfully delivered SIGHUP, but the semantics of Child::kill
            // are that on success the process is dead or shortly about to
            // terminate.  Since SIGUP doesn't guarantee termination, we
            // give the process a bit of a grace period to shutdown or do whatever
            // it is doing in its signal handler befre we proceed with the
            // full on kill.
            for attempt in 0..5 {
                if attempt > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                if let Ok(Some(_)) = self.try_wait() {
                    // It completed, so report success!
                    return Ok(());
                }
            }

            // it's still alive after a grace period, so proceed with a kill
        }

        std::process::Child::kill(self)
    }

    #[cfg(windows)]
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        struct RawDup(RawHandle);
        impl AsRawHandle for RawDup {
            fn as_raw_handle(&self) -> RawHandle {
                self.0
            }
        }

        Box::new(ProcessSignaller {
            pid: self.process_id(),
            handle: Child::as_raw_handle(self)
                .as_ref()
                .and_then(|h| filedescriptor::OwnedHandle::dup(&RawDup(*h)).ok()),
        })
    }

    #[cfg(unix)]
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(ProcessSignaller {
            pid: self.process_id(),
        })
    }
}

pub fn native_pty_system() -> Box<dyn PtySystem + Send> {
    Box::new(NativePtySystem::default())
}

#[cfg(unix)]
pub type NativePtySystem = unix::UnixPtySystem;
#[cfg(windows)]
pub type NativePtySystem = win::conpty::ConPtySystem;

#[cfg(test)]
mod tests {
    use super::*;

    // ── PtySize ─────────────────────────────────────────────

    #[test]
    fn pty_size_default() {
        let s = PtySize::default();
        assert_eq!(s.rows, 24);
        assert_eq!(s.cols, 80);
        assert_eq!(s.pixel_width, 0);
        assert_eq!(s.pixel_height, 0);
    }

    #[test]
    fn pty_size_clone_eq() {
        let a = PtySize {
            rows: 50,
            cols: 132,
            pixel_width: 800,
            pixel_height: 600,
        };
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn pty_size_debug() {
        let s = PtySize::default();
        let dbg = format!("{s:?}");
        assert!(dbg.contains("PtySize"));
        assert!(dbg.contains("24"));
    }

    #[test]
    fn pty_size_ne() {
        let a = PtySize::default();
        let b = PtySize {
            rows: 25,
            ..PtySize::default()
        };
        assert_ne!(a, b);
    }

    // ── ExitStatus: with_exit_code ──────────────────────────

    #[test]
    fn exit_status_success_zero() {
        let s = ExitStatus::with_exit_code(0);
        assert!(s.success());
        assert_eq!(s.exit_code(), 0);
        assert!(s.signal().is_none());
    }

    #[test]
    fn exit_status_failure_nonzero() {
        let s = ExitStatus::with_exit_code(1);
        assert!(!s.success());
        assert_eq!(s.exit_code(), 1);
    }

    #[test]
    fn exit_status_arbitrary_code() {
        let s = ExitStatus::with_exit_code(127);
        assert!(!s.success());
        assert_eq!(s.exit_code(), 127);
    }

    // ── ExitStatus: with_signal ─────────────────────────────

    #[test]
    fn exit_status_with_signal() {
        let s = ExitStatus::with_signal("SIGTERM");
        assert!(!s.success());
        assert_eq!(s.exit_code(), 1);
        assert_eq!(s.signal(), Some("SIGTERM"));
    }

    #[test]
    fn exit_status_signal_is_never_success() {
        let s = ExitStatus::with_signal("SIGHUP");
        assert!(!s.success());
    }

    // ── ExitStatus: Display ─────────────────────────────────

    #[test]
    fn exit_status_display_success() {
        let s = ExitStatus::with_exit_code(0);
        assert_eq!(format!("{s}"), "Success");
    }

    #[test]
    fn exit_status_display_exit_code() {
        let s = ExitStatus::with_exit_code(42);
        assert_eq!(format!("{s}"), "Exited with code 42");
    }

    #[test]
    fn exit_status_display_signal() {
        let s = ExitStatus::with_signal("SIGKILL");
        assert_eq!(format!("{s}"), "Terminated by SIGKILL");
    }

    // ── ExitStatus: Clone / Debug ───────────────────────────

    #[test]
    fn exit_status_clone() {
        let a = ExitStatus::with_exit_code(5);
        let b = a.clone();
        assert_eq!(a.exit_code(), b.exit_code());
        assert_eq!(a.signal(), b.signal());
    }

    #[test]
    fn exit_status_debug() {
        let s = ExitStatus::with_signal("SIGINT");
        let dbg = format!("{s:?}");
        assert!(dbg.contains("SIGINT"));
    }

    // ── Second-pass expansion ────────────────────────────────────

    // ── PtySize additional ────────────────────────────────────

    #[test]
    fn pty_size_all_fields_custom() {
        let s = PtySize {
            rows: 100,
            cols: 200,
            pixel_width: 1600,
            pixel_height: 1200,
        };
        assert_eq!(s.rows, 100);
        assert_eq!(s.cols, 200);
        assert_eq!(s.pixel_width, 1600);
        assert_eq!(s.pixel_height, 1200);
    }

    #[test]
    fn pty_size_zero_dimensions() {
        let s = PtySize {
            rows: 0,
            cols: 0,
            pixel_width: 0,
            pixel_height: 0,
        };
        assert_eq!(s.rows, 0);
        assert_eq!(s.cols, 0);
    }

    #[test]
    fn pty_size_max_u16() {
        let s = PtySize {
            rows: u16::MAX,
            cols: u16::MAX,
            pixel_width: u16::MAX,
            pixel_height: u16::MAX,
        };
        assert_eq!(s.rows, u16::MAX);
    }

    #[test]
    fn pty_size_copy_is_independent() {
        let a = PtySize {
            rows: 10,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        };
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn pty_size_ne_cols() {
        let a = PtySize::default();
        let b = PtySize { cols: 132, ..a };
        assert_ne!(a, b);
    }

    #[test]
    fn pty_size_ne_pixel_width() {
        let a = PtySize::default();
        let b = PtySize {
            pixel_width: 100,
            ..a
        };
        assert_ne!(a, b);
    }

    #[test]
    fn pty_size_ne_pixel_height() {
        let a = PtySize::default();
        let b = PtySize {
            pixel_height: 100,
            ..a
        };
        assert_ne!(a, b);
    }

    #[test]
    fn pty_size_debug_contains_all_fields() {
        let s = PtySize {
            rows: 50,
            cols: 132,
            pixel_width: 800,
            pixel_height: 600,
        };
        let dbg = format!("{s:?}");
        assert!(dbg.contains("50"));
        assert!(dbg.contains("132"));
        assert!(dbg.contains("800"));
        assert!(dbg.contains("600"));
    }

    // ── ExitStatus additional ─────────────────────────────────

    #[test]
    fn exit_status_code_255() {
        let s = ExitStatus::with_exit_code(255);
        assert!(!s.success());
        assert_eq!(s.exit_code(), 255);
    }

    #[test]
    fn exit_status_code_max_u32() {
        let s = ExitStatus::with_exit_code(u32::MAX);
        assert!(!s.success());
        assert_eq!(s.exit_code(), u32::MAX);
    }

    #[test]
    fn exit_status_signal_empty_string() {
        let s = ExitStatus::with_signal("");
        assert!(!s.success());
        assert_eq!(s.signal(), Some(""));
    }

    #[test]
    fn exit_status_display_empty_signal() {
        let s = ExitStatus::with_signal("");
        assert_eq!(format!("{s}"), "Terminated by ");
    }

    #[test]
    fn exit_status_signal_has_code_1() {
        let s = ExitStatus::with_signal("SIGTERM");
        assert_eq!(s.exit_code(), 1);
    }

    #[test]
    fn exit_status_clone_with_signal() {
        let a = ExitStatus::with_signal("SIGKILL");
        let b = a.clone();
        assert_eq!(a.exit_code(), b.exit_code());
        assert_eq!(a.signal(), b.signal());
    }

    #[test]
    fn exit_status_debug_success() {
        let s = ExitStatus::with_exit_code(0);
        let dbg = format!("{s:?}");
        assert!(dbg.contains("0"));
    }

    #[test]
    fn exit_status_display_code_1() {
        let s = ExitStatus::with_exit_code(1);
        assert_eq!(format!("{s}"), "Exited with code 1");
    }

    #[test]
    fn exit_status_no_signal_by_default() {
        let s = ExitStatus::with_exit_code(0);
        assert!(s.signal().is_none());
    }

    #[test]
    fn exit_status_success_is_only_zero() {
        for code in 1..=10u32 {
            assert!(!ExitStatus::with_exit_code(code).success());
        }
    }

    #[test]
    fn exit_status_display_various_codes() {
        assert_eq!(
            format!("{}", ExitStatus::with_exit_code(127)),
            "Exited with code 127"
        );
        assert_eq!(
            format!("{}", ExitStatus::with_exit_code(130)),
            "Exited with code 130"
        );
    }

    #[test]
    fn exit_status_display_various_signals() {
        assert_eq!(
            format!("{}", ExitStatus::with_signal("SIGSEGV")),
            "Terminated by SIGSEGV"
        );
        assert_eq!(
            format!("{}", ExitStatus::with_signal("SIGPIPE")),
            "Terminated by SIGPIPE"
        );
    }

    #[test]
    fn exit_status_clone_preserves_signal_none() {
        let a = ExitStatus::with_exit_code(42);
        let b = a.clone();
        assert!(b.signal().is_none());
        assert_eq!(b.exit_code(), 42);
    }

    #[test]
    fn pty_size_eq_reflexive() {
        let s = PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 10,
            pixel_height: 20,
        };
        assert_eq!(s, s);
    }

    // ── Third-pass expansion ────────────────────────────────────

    #[test]
    fn exit_status_with_signal_code_is_1() {
        let s = ExitStatus::with_signal("SIGKILL");
        assert_eq!(s.exit_code(), 1);
    }

    #[test]
    fn exit_status_with_signal_is_not_success() {
        let s = ExitStatus::with_signal("SIGTERM");
        assert!(!s.success());
    }

    #[test]
    fn exit_status_signal_name_matches() {
        let s = ExitStatus::with_signal("SIGUSR1");
        assert_eq!(s.signal(), Some("SIGUSR1"));
    }

    #[test]
    fn exit_status_exit_code_no_signal() {
        let s = ExitStatus::with_exit_code(42);
        assert!(s.signal().is_none());
    }

    #[test]
    fn exit_status_display_zero_is_success_text() {
        assert_eq!(format!("{}", ExitStatus::with_exit_code(0)), "Success");
    }

    #[test]
    fn exit_status_display_sigint_terminated() {
        assert_eq!(
            format!("{}", ExitStatus::with_signal("SIGINT")),
            "Terminated by SIGINT"
        );
    }

    #[test]
    fn exit_status_clone_preserves_signal_some() {
        let a = ExitStatus::with_signal("SIGHUP");
        let b = a.clone();
        assert_eq!(b.signal(), Some("SIGHUP"));
        assert!(!b.success());
    }

    #[test]
    fn pty_size_default_rows_cols_pixels() {
        let s = PtySize::default();
        assert_eq!(s.rows, 24);
        assert_eq!(s.cols, 80);
        assert_eq!(s.pixel_width, 0);
        assert_eq!(s.pixel_height, 0);
    }

    #[test]
    fn pty_size_rows_differ_ne() {
        let a = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let b = PtySize { rows: 25, cols: 80, pixel_width: 0, pixel_height: 0 };
        assert_ne!(a, b);
    }

    #[test]
    fn pty_size_cols_differ_ne() {
        let a = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let b = PtySize { rows: 24, cols: 120, pixel_width: 0, pixel_height: 0 };
        assert_ne!(a, b);
    }

    #[test]
    fn pty_size_copy_both_survive() {
        let a = PtySize { rows: 10, cols: 20, pixel_width: 5, pixel_height: 5 };
        let b = a; // Copy
        let c = a; // Still valid
        assert_eq!(b, c);
    }

    #[test]
    fn pty_size_debug_shows_row_col_fields() {
        let s = PtySize { rows: 1, cols: 2, pixel_width: 3, pixel_height: 4 };
        let dbg = format!("{s:?}");
        assert!(dbg.contains("rows: 1"));
        assert!(dbg.contains("cols: 2"));
    }

    #[test]
    fn exit_status_debug_contains_struct_name() {
        let s = ExitStatus::with_exit_code(0);
        let dbg = format!("{s:?}");
        assert!(dbg.contains("ExitStatus"));
    }

    #[test]
    fn exit_status_u32_max_code() {
        let s = ExitStatus::with_exit_code(u32::MAX);
        assert_eq!(s.exit_code(), u32::MAX);
        assert!(!s.success());
    }
}
