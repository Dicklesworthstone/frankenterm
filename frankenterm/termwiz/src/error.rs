use std::fmt::Display;
use thiserror::*;

/// The termwiz Error type encapsulates a range of internal
/// errors in an opaque manner.  You can use the `source`
/// method to reach the underlying errors if
/// necessary, but it is not expected that most code will
/// need to do so.  Please file an issue if you've got a
/// usecase for this!
#[derive(Error, Debug)]
#[error(transparent)]
pub struct Error(pub(crate) InternalError);

/// A Result whose error type is a termwiz Error
pub type Result<T> = std::result::Result<T, Error>;

impl<E> From<E> for Error
where
    E: Into<InternalError>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

/// This enum encapsulates the various errors that can be
/// mapped into the termwiz Error type.
/// The intent is that this is effectively private to termwiz
/// itself, but since Rust doesn't allow enums with private
/// variants, we're dancing around with a newtype of an enum
/// and hiding it from the docs.
#[derive(Error, Debug)]
#[non_exhaustive]
#[doc(hidden)]
pub enum InternalError {
    #[error(transparent)]
    Fmt(#[from] std::fmt::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Regex(#[from] fancy_regex::Error),

    #[error(transparent)]
    FromUtf8(#[from] std::string::FromUtf8Error),

    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),

    #[error(transparent)]
    ParseFloat(#[from] std::num::ParseFloatError),

    #[error(transparent)]
    ParseInt(#[from] std::num::ParseIntError),

    #[error(transparent)]
    FloatIsNan(#[from] ordered_float::FloatIsNan),

    #[error("{0}")]
    StringErr(#[from] StringWrap),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),

    #[error(transparent)]
    Terminfo(#[from] terminfo::Error),

    #[error(transparent)]
    FileDescriptor(#[from] filedescriptor::Error),

    #[cfg(feature = "use_image")]
    #[error(transparent)]
    ImageCellError(#[from] frankenterm_cell::image::ImageCellError),

    #[cfg(feature = "image")]
    #[error(transparent)]
    BlobLease(#[from] frankenterm_blob_leases::Error),

    #[error("{}", .context)]
    Context {
        context: String,
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

impl From<String> for InternalError {
    fn from(s: String) -> Self {
        InternalError::StringErr(StringWrap(s))
    }
}

#[derive(Error, Debug)]
#[doc(hidden)]
#[error("{0}")]
pub struct StringWrap(pub String);

#[macro_export]
macro_rules! format_err {
    ($msg:literal $(,)?) => {
        return $crate::error::Error::from($crate::error::StringWrap($msg.to_string()))
    };
    ($err:expr $(,)?) => {
        return $crate::error::Error::from($crate::error::StringWrap(format!($err)))
    };
    ($fmt:expr, $($arg:tt)*) => {
        return $crate::error::Error::from($crate::error::StringWrap(format!($fmt, $($arg)*)))
    };
}

#[macro_export]
macro_rules! bail {
    ($msg:literal $(,)?) => {
        return Err($crate::error::StringWrap($msg.to_string()).into())
    };
    ($err:expr $(,)?) => {
        return Err($crate::error::StringWrap(format!($err)).into())
    };
    ($fmt:expr, $($arg:tt)*) => {
        return Err($crate::error::StringWrap(format!($fmt, $($arg)*)).into())
    };
}

#[macro_export]
macro_rules! ensure {
    ($cond:expr, $msg:literal $(,)?) => {
        if !$cond {
            return Err($crate::error::StringWrap(format!($msg)).into());
        }
    };
    ($cond:expr, $err:expr $(,)?) => {
        if !$cond {
            return Err($crate::error::StringWrap(format!($err)).into());
        }
    };
    ($cond:expr, $fmt:expr, $($arg:tt)*) => {
        if !$cond {
            return Err($crate::error::StringWrap(format!($fmt, $($arg)*)).into());
        }
    };
}

/// This trait allows extending the Result type so that it can create a
/// `termwiz::Error` that wraps an underlying other error and provide
/// additional context on that error.
pub trait Context<T, E> {
    /// Wrap the error value with additional context.
    fn context<C>(self, context: C) -> Result<T>
    where
        C: Display + Send + Sync + 'static;

    /// Wrap the error value with additional context that is evaluated lazily
    /// only once an error does occur.
    fn with_context<C, F>(self, f: F) -> Result<T>
    where
        C: Display + Send + Sync + 'static,
        F: FnOnce() -> C;
}

impl<T, E> Context<T, E> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn context<C>(self, context: C) -> Result<T>
    where
        C: Display + Send + Sync + 'static,
    {
        self.map_err(|error| {
            Error(InternalError::Context {
                context: context.to_string(),
                source: Box::new(error),
            })
        })
    }

    fn with_context<C, F>(self, context: F) -> Result<T>
    where
        C: Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        self.map_err(|error| {
            Error(InternalError::Context {
                context: context().to_string(),
                source: Box::new(error),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as StdError;

    // ── StringWrap ──────────────────────────────────────────

    #[test]
    fn string_wrap_displays_inner_string() {
        let sw = StringWrap("test error".to_string());
        assert_eq!(format!("{sw}"), "test error");
    }

    #[test]
    fn string_wrap_debug() {
        let sw = StringWrap("msg".to_string());
        let debug = format!("{sw:?}");
        assert!(debug.contains("msg"));
    }

    // ── InternalError ───────────────────────────────────────

    #[test]
    fn internal_error_from_string() {
        let err: InternalError = "something failed".to_string().into();
        assert_eq!(format!("{err}"), "something failed");
    }

    #[test]
    fn internal_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let err: InternalError = io_err.into();
        assert!(format!("{err}").contains("not found"));
    }

    #[test]
    fn internal_error_from_fmt_error() {
        let fmt_err = std::fmt::Error;
        let err: InternalError = fmt_err.into();
        let display = format!("{err}");
        assert!(!display.is_empty());
    }

    #[test]
    fn internal_error_context_displays_context() {
        let ctx_err = InternalError::Context {
            context: "while doing X".to_string(),
            source: Box::new(std::io::Error::new(std::io::ErrorKind::Other, "inner")),
        };
        assert_eq!(format!("{ctx_err}"), "while doing X");
    }

    // ── Error wrapper ───────────────────────────────────────

    #[test]
    fn error_from_internal_is_transparent() {
        let internal: InternalError = "custom msg".to_string().into();
        let err = Error(internal);
        assert_eq!(format!("{err}"), "custom msg");
    }

    #[test]
    fn error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let err: Error = io_err.into();
        assert!(format!("{err}").contains("broken"));
    }

    #[test]
    fn error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    #[test]
    fn error_debug_is_not_empty() {
        let err: Error = std::io::Error::new(std::io::ErrorKind::Other, "test").into();
        let debug = format!("{err:?}");
        assert!(!debug.is_empty());
    }

    #[test]
    fn error_transparent_delegates_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "root cause");
        let err: Error = io_err.into();
        // Error is transparent, so Display delegates to InternalError
        assert!(format!("{err}").contains("root cause"));
    }

    // ── Context trait ───────────────────────────────────────

    #[test]
    fn context_wraps_error() {
        let result: std::result::Result<(), std::io::Error> =
            Err(std::io::Error::new(std::io::ErrorKind::Other, "inner"));
        let contexted = result.context("outer context");
        let err = contexted.unwrap_err();
        assert_eq!(format!("{err}"), "outer context");
    }

    #[test]
    fn with_context_wraps_error_lazily() {
        let result: std::result::Result<(), std::io::Error> =
            Err(std::io::Error::new(std::io::ErrorKind::Other, "inner"));
        let contexted = result.with_context(|| format!("lazy {}", "context"));
        let err = contexted.unwrap_err();
        assert_eq!(format!("{err}"), "lazy context");
    }

    #[test]
    fn context_preserves_ok() {
        let result: std::result::Result<i32, std::io::Error> = Ok(42);
        let contexted = result.context("should not appear");
        assert_eq!(contexted.unwrap(), 42);
    }

    #[test]
    fn with_context_does_not_call_closure_on_ok() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static CALLED: AtomicBool = AtomicBool::new(false);
        let result: std::result::Result<i32, std::io::Error> = Ok(42);
        let contexted = result.with_context(|| {
            CALLED.store(true, Ordering::SeqCst);
            "should not appear"
        });
        assert_eq!(contexted.unwrap(), 42);
        assert!(!CALLED.load(Ordering::SeqCst));
    }

    // ── bail! macro ─────────────────────────────────────────

    fn bail_literal() -> Result<()> {
        bail!("literal error");
    }

    fn bail_expr() -> Result<()> {
        let msg = "dynamic";
        bail!("{} error", msg);
    }

    #[test]
    fn bail_returns_error_with_literal() {
        let err = bail_literal().unwrap_err();
        assert_eq!(format!("{err}"), "literal error");
    }

    #[test]
    fn bail_returns_error_with_format() {
        let err = bail_expr().unwrap_err();
        assert_eq!(format!("{err}"), "dynamic error");
    }

    // ── ensure! macro ───────────────────────────────────────

    fn ensure_passes(val: bool) -> Result<()> {
        ensure!(val, "condition failed");
        Ok(())
    }

    #[test]
    fn ensure_true_is_ok() {
        assert!(ensure_passes(true).is_ok());
    }

    #[test]
    fn ensure_false_returns_error() {
        let err = ensure_passes(false).unwrap_err();
        assert_eq!(format!("{err}"), "condition failed");
    }
}
