use core::fmt::Display;

use crate::allocate::*;

/// The termwiz Error type encapsulates a range of internal
/// errors in an opaque manner.  You can use the `source`
/// method to reach the underlying errors if
/// necessary, but it is not expected that most code will
/// need to do so.  Please file an issue if you've got a
/// usecase for this!
#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub struct Error(pub(crate) InternalError);

/// A Result whose error type is a termwiz Error
pub type Result<T> = core::result::Result<T, Error>;

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
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
#[doc(hidden)]
pub enum InternalError {
    #[error(transparent)]
    Fmt(#[from] core::fmt::Error),

    #[cfg(feature = "std")]
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[cfg(feature = "std")]
    #[error(transparent)]
    FromUtf8(#[from] std::string::FromUtf8Error),
    #[cfg(not(feature = "std"))]
    #[error(transparent)]
    FromUtf8(#[from] alloc::string::FromUtf8Error),

    #[error(transparent)]
    Utf8(#[from] core::str::Utf8Error),

    #[error(transparent)]
    ParseFloat(#[from] core::num::ParseFloatError),

    #[error(transparent)]
    ParseInt(#[from] core::num::ParseIntError),

    #[error("{0}")]
    StringErr(#[from] StringWrap),

    #[cfg(feature = "image")]
    #[error(transparent)]
    BlobLease(#[from] frankenterm_blob_leases::Error),

    #[cfg(feature = "use_image")]
    #[error(transparent)]
    ImageError(#[from] image::ImageError),

    #[cfg(feature = "tmux_cc")]
    #[error(transparent)]
    Pest(#[from] pest::error::Error<crate::tmux_cc::parser::Rule>),

    #[error("{}", .context)]
    Context {
        context: String,
        source: Box<dyn core::error::Error + Send + Sync + 'static>,
    },
}

impl From<String> for InternalError {
    fn from(s: String) -> Self {
        InternalError::StringErr(StringWrap(s))
    }
}

#[derive(thiserror::Error, Debug)]
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

impl<T, E> Context<T, E> for core::result::Result<T, E>
where
    E: core::error::Error + Send + Sync + 'static,
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

    #[test]
    fn string_wrap_display() {
        let sw = StringWrap("hello world".to_string());
        assert_eq!(format!("{}", sw), "hello world");
    }

    #[test]
    fn string_wrap_debug() {
        let sw = StringWrap("test".to_string());
        let dbg = format!("{:?}", sw);
        assert!(dbg.contains("test"));
    }

    #[test]
    fn internal_error_from_string() {
        let err: InternalError = "something went wrong".to_string().into();
        let msg = format!("{}", err);
        assert_eq!(msg, "something went wrong");
    }

    #[test]
    fn internal_error_from_fmt_error() {
        let fmt_err = core::fmt::Error;
        let err: InternalError = fmt_err.into();
        let msg = format!("{}", err);
        assert!(!msg.is_empty());
    }

    #[test]
    fn internal_error_from_utf8_error() {
        let bad_bytes = vec![0xff, 0xfe];
        let utf8_err = alloc::string::String::from_utf8(bad_bytes).unwrap_err();
        let err: InternalError = utf8_err.into();
        let msg = format!("{}", err);
        assert!(msg.contains("invalid"));
    }

    #[test]
    fn internal_error_from_parse_float() {
        let pf_err: core::num::ParseFloatError = "notfloat".parse::<f64>().unwrap_err();
        let err: InternalError = pf_err.into();
        let msg = format!("{}", err);
        assert!(!msg.is_empty());
    }

    #[test]
    fn internal_error_from_parse_int() {
        let pi_err: core::num::ParseIntError = "notint".parse::<i32>().unwrap_err();
        let err: InternalError = pi_err.into();
        let msg = format!("{}", err);
        assert!(!msg.is_empty());
    }

    #[test]
    fn error_from_internal_error() {
        let internal = InternalError::from("test error".to_string());
        let err = Error(internal);
        let msg = format!("{}", err);
        assert_eq!(msg, "test error");
    }

    #[test]
    fn error_debug() {
        let err = Error(InternalError::from("debug test".to_string()));
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("debug test"));
    }

    #[test]
    fn error_from_string_wrap() {
        let sw = StringWrap("wrapped".to_string());
        let err: Error = sw.into();
        let msg = format!("{}", err);
        assert_eq!(msg, "wrapped");
    }

    #[test]
    fn error_from_fmt_error() {
        let fmt_err = core::fmt::Error;
        let err: Error = fmt_err.into();
        let msg = format!("{}", err);
        assert!(!msg.is_empty());
    }

    #[test]
    fn context_on_err_result() {
        let result: core::result::Result<(), core::fmt::Error> = Err(core::fmt::Error);
        let contexted = result.context("failed to format");
        assert!(contexted.is_err());
        let err = contexted.unwrap_err();
        let msg = format!("{}", err);
        assert_eq!(msg, "failed to format");
    }

    #[test]
    fn context_on_ok_result() {
        let result: core::result::Result<i32, core::fmt::Error> = Ok(42);
        let contexted = result.context("should not appear");
        assert_eq!(contexted.unwrap(), 42);
    }

    #[test]
    fn with_context_on_err_result() {
        let result: core::result::Result<(), core::fmt::Error> = Err(core::fmt::Error);
        let contexted = result.with_context(|| "lazy context message");
        assert!(contexted.is_err());
        let err = contexted.unwrap_err();
        let msg = format!("{}", err);
        assert_eq!(msg, "lazy context message");
    }

    #[test]
    fn with_context_on_ok_result() {
        let result: core::result::Result<i32, core::fmt::Error> = Ok(99);
        let contexted = result.with_context(|| "should not evaluate");
        assert_eq!(contexted.unwrap(), 99);
    }

    #[test]
    fn internal_error_context_variant() {
        let source_err = core::fmt::Error;
        let err = InternalError::Context {
            context: "outer context".to_string(),
            source: Box::new(source_err),
        };
        let msg = format!("{}", err);
        assert_eq!(msg, "outer context");
    }

    #[test]
    fn internal_error_debug() {
        let err = InternalError::from("debug variant".to_string());
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("StringErr"));
    }
}
