//! An abstraction over a terminal device

use crate::caps::probed::ProbeCapabilities;
use crate::caps::Capabilities;
use crate::input::InputEvent;
use crate::surface::Change;
use crate::{format_err, Result};
use num_traits::NumCast;
use std::fmt::Display;
use std::time::Duration;

#[cfg(feature = "use_serde")]
use serde::Deserialize;
#[cfg(feature = "use_serde")]
use serde::Serialize;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

pub mod buffered;

#[cfg(unix)]
pub use self::unix::{UnixTerminal, UnixTerminalWaker as TerminalWaker};
#[cfg(windows)]
pub use self::windows::{WindowsTerminal, WindowsTerminalWaker as TerminalWaker};

/// Represents the size of the terminal screen.
/// The number of rows and columns of character cells are expressed.
/// Some implementations populate the size of those cells in pixels.
// On Windows, GetConsoleFontSize() can return the size of a cell in
// logical units and we can probably use this to populate xpixel, ypixel.
// GetConsoleScreenBufferInfo() can return the rows and cols.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenSize {
    /// The number of rows of text
    pub rows: usize,
    /// The number of columns per row
    pub cols: usize,
    /// The width of a cell in pixels.  Some implementations never
    /// set this to anything other than zero.
    pub xpixel: usize,
    /// The height of a cell in pixels.  Some implementations never
    /// set this to anything other than zero.
    pub ypixel: usize,
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocking {
    DoNotWait,
    Wait,
}

/// `Terminal` abstracts over some basic terminal capabilities.
/// If the `set_raw_mode` or `set_cooked_mode` functions are used in
/// any combination, the implementation is required to restore the
/// terminal mode that was in effect when it was created.
pub trait Terminal {
    /// Raw mode disables input line buffering, allowing data to be
    /// read as the user presses keys, disables local echo, so keys
    /// pressed by the user do not implicitly render to the terminal
    /// output, and disables canonicalization of unix newlines to CRLF.
    fn set_raw_mode(&mut self) -> Result<()>;
    fn set_cooked_mode(&mut self) -> Result<()>;

    /// Enter the alternate screen.  The alternate screen will be left
    /// automatically when the `Terminal` is dropped.
    fn enter_alternate_screen(&mut self) -> Result<()>;

    /// Exit the alternate screen.
    fn exit_alternate_screen(&mut self) -> Result<()>;

    /// Queries the current screen size, returning width, height.
    fn get_screen_size(&mut self) -> Result<ScreenSize>;

    /// Returns a capability probing helper that will use escape
    /// sequences to attempt to probe information from the terminal
    fn probe_capabilities(&mut self) -> Option<ProbeCapabilities<'_>> {
        None
    }

    /// Sets the current screen size
    fn set_screen_size(&mut self, size: ScreenSize) -> Result<()>;

    /// Render a series of changes to the terminal output
    fn render(&mut self, changes: &[Change]) -> Result<()>;

    /// Flush any buffered output
    fn flush(&mut self) -> Result<()>;

    /// Check for a parsed input event.
    /// `wait` indicates the behavior in the case that no input is
    /// immediately available.  If wait is `None` then `poll_input`
    /// will not return until an event is available.  If wait is
    /// `Some(duration)` then `poll_input` will wait up to the given
    /// duration for an event before returning with a value of
    /// `Ok(None)`.  If wait is `Some(Duration::ZERO)` then the
    /// poll is non-blocking.
    ///
    /// The possible values returned as `InputEvent`s depend on the
    /// mode of the terminal.  Most values are not returned unless
    /// the terminal is set to raw mode.
    fn poll_input(&mut self, wait: Option<Duration>) -> Result<Option<InputEvent>>;

    fn waker(&self) -> TerminalWaker;
}

/// `SystemTerminal` is a concrete implementation of `Terminal`.
/// Ideally you wouldn't reference `SystemTerminal` in consuming
/// code.  This type is exposed for convenience if you are doing
/// something unusual and want easier access to the constructors.
#[cfg(unix)]
pub type SystemTerminal = UnixTerminal;
#[cfg(windows)]
pub type SystemTerminal = WindowsTerminal;

/// Construct a new instance of Terminal.
/// The terminal will have a renderer that is influenced by the configuration
/// in the provided `Capabilities` instance.
/// The terminal will explicitly open `/dev/tty` on Unix systems and
/// `CONIN$` and `CONOUT$` on Windows systems, so that it should yield a
/// functioning console with minimal headaches.
/// If you have a more advanced use case you will want to look to the
/// constructors for `UnixTerminal` and `WindowsTerminal` and call whichever
/// one is most suitable for your needs.
pub fn new_terminal(caps: Capabilities) -> Result<impl Terminal> {
    SystemTerminal::new(caps)
}

pub(crate) fn cast<T: NumCast + Display + Copy, U: NumCast>(n: T) -> Result<U> {
    num_traits::cast(n).ok_or_else(|| format_err!("{} is out of bounds for this system", n))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ScreenSize ────────────────────────────────────

    #[test]
    fn screen_size_construction() {
        let s = ScreenSize {
            rows: 24,
            cols: 80,
            xpixel: 8,
            ypixel: 16,
        };
        assert_eq!(s.rows, 24);
        assert_eq!(s.cols, 80);
        assert_eq!(s.xpixel, 8);
        assert_eq!(s.ypixel, 16);
    }

    #[test]
    fn screen_size_eq() {
        let a = ScreenSize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        let b = ScreenSize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn screen_size_ne_rows() {
        let a = ScreenSize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        let b = ScreenSize {
            rows: 25,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn screen_size_ne_cols() {
        let a = ScreenSize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        let b = ScreenSize {
            rows: 24,
            cols: 132,
            xpixel: 0,
            ypixel: 0,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn screen_size_ne_pixels() {
        let a = ScreenSize {
            rows: 24,
            cols: 80,
            xpixel: 8,
            ypixel: 16,
        };
        let b = ScreenSize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn screen_size_clone_copy() {
        let a = ScreenSize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn screen_size_debug() {
        let s = ScreenSize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("ScreenSize"));
        assert!(dbg.contains("24"));
        assert!(dbg.contains("80"));
    }

    #[test]
    fn screen_size_zero() {
        let s = ScreenSize {
            rows: 0,
            cols: 0,
            xpixel: 0,
            ypixel: 0,
        };
        assert_eq!(s.rows, 0);
        assert_eq!(s.cols, 0);
    }

    // ── Blocking ──────────────────────────────────────

    #[test]
    fn blocking_do_not_wait() {
        let b = Blocking::DoNotWait;
        assert!(format!("{:?}", b).contains("DoNotWait"));
    }

    #[test]
    fn blocking_wait() {
        let b = Blocking::Wait;
        assert!(format!("{:?}", b).contains("Wait"));
    }

    #[test]
    fn blocking_eq() {
        assert_eq!(Blocking::Wait, Blocking::Wait);
        assert_eq!(Blocking::DoNotWait, Blocking::DoNotWait);
    }

    #[test]
    fn blocking_ne() {
        assert_ne!(Blocking::Wait, Blocking::DoNotWait);
    }

    #[test]
    fn blocking_clone_copy() {
        let b = Blocking::Wait;
        let c = b;
        assert_eq!(b, c);
    }

    // ── cast ──────────────────────────────────────────

    #[test]
    fn cast_u32_to_usize() {
        let result: usize = cast(42u32).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn cast_i32_to_u32() {
        let result: u32 = cast(100i32).unwrap();
        assert_eq!(result, 100);
    }

    #[test]
    fn cast_negative_to_unsigned_fails() {
        let result: Result<u32> = cast(-1i32);
        assert!(result.is_err());
    }

    #[test]
    fn cast_zero() {
        let result: u8 = cast(0u32).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn cast_overflow_fails() {
        let result: Result<u8> = cast(256u32);
        assert!(result.is_err());
    }

    #[test]
    fn cast_max_u8() {
        let result: u8 = cast(255u32).unwrap();
        assert_eq!(result, 255);
    }

    #[test]
    fn cast_error_message_contains_value() {
        let result: Result<u8> = cast(999u32);
        let err = result.unwrap_err();
        assert!(err.to_string().contains("999"));
    }
}
