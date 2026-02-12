//! Outcome<T,E> adapter layer for the asupersync migration.
//!
//! # Strategy
//!
//! - **Internal async code** uses `Outcome<T, E>` natively, gaining
//!   structured cancellation (`Cancelled`) and panic propagation (`Panicked`).
//! - **Public API boundaries** convert to/from `Result<T, crate::Error>`,
//!   minimising blast radius while gaining cancel-correctness internally.
//!
//! # Key Types
//!
//! - [`FtOutcome<T>`]: Convenience alias for `Outcome<T, crate::Error>`.
//! - [`OutcomeExt`]: Extension trait with ft-specific helpers.
//! - [`try_outcome!`]: Macro emulating `?` for Outcome (since Outcome
//!   does not implement the unstable `Try` trait).

// Re-export asupersync types for downstream consumers
pub use asupersync::{CancelKind, CancelReason, Outcome, OutcomeError, PanicPayload, Severity};

use crate::Error;

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

/// Convenience alias: an Outcome whose error type is `crate::Error`.
pub type FtOutcome<T> = Outcome<T, Error>;

// ---------------------------------------------------------------------------
// try_outcome! macro — ? operator for Outcome
// ---------------------------------------------------------------------------

/// Unwrap an `Outcome::Ok(v)`, or early-return the non-Ok variant.
///
/// Works analogously to `?` on `Result`: if the expression evaluates to
/// `Outcome::Err`, `Outcome::Cancelled`, or `Outcome::Panicked`, the
/// enclosing function returns that variant immediately.
///
/// # Example
///
/// ```ignore
/// fn process(data: &[u8]) -> FtOutcome<usize> {
///     let parsed = try_outcome!(parse(data));
///     Outcome::ok(parsed.len())
/// }
/// ```
#[macro_export]
macro_rules! try_outcome {
    ($expr:expr) => {
        match $expr {
            ::asupersync::Outcome::Ok(v) => v,
            ::asupersync::Outcome::Err(e) => return ::asupersync::Outcome::Err(e.into()),
            ::asupersync::Outcome::Cancelled(r) => {
                return ::asupersync::Outcome::Cancelled(r);
            }
            ::asupersync::Outcome::Panicked(p) => {
                return ::asupersync::Outcome::Panicked(p);
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Outcome → Result conversion
// ---------------------------------------------------------------------------

/// Convert an `Outcome<T, E>` into a `Result<T, E>`, mapping `Cancelled`
/// and `Panicked` into `E` via the provided closures.
///
/// Prefer this at public API boundaries where callers expect `Result`.
pub fn outcome_into_result<T, E>(
    outcome: Outcome<T, E>,
    on_cancelled: impl FnOnce(CancelReason) -> E,
    on_panicked: impl FnOnce(PanicPayload) -> E,
) -> Result<T, E> {
    match outcome {
        Outcome::Ok(v) => Ok(v),
        Outcome::Err(e) => Err(e),
        Outcome::Cancelled(r) => Err(on_cancelled(r)),
        Outcome::Panicked(p) => Err(on_panicked(p)),
    }
}

/// Convert an `FtOutcome<T>` into `crate::Result<T>`, mapping cancellation
/// into `Error::Cancelled` and panics into `Error::Panicked`.
///
/// This is the standard boundary conversion for ft public APIs.
pub fn ft_outcome_to_result<T>(outcome: FtOutcome<T>) -> crate::Result<T> {
    outcome_into_result(
        outcome,
        |reason| Error::Cancelled(format!("{}", reason.kind)),
        |payload| Error::Panicked(payload.message().to_string()),
    )
}

// ---------------------------------------------------------------------------
// Result → Outcome conversion
// ---------------------------------------------------------------------------

/// Convert a `Result<T, E>` into an `Outcome<T, E>`.
///
/// This is a thin wrapper around the `From` impl that asupersync already
/// provides, offered here for discoverability.
pub fn result_to_outcome<T, E>(result: Result<T, E>) -> Outcome<T, E> {
    Outcome::from(result)
}

/// Convert a `crate::Result<T>` into an `FtOutcome<T>`.
pub fn ft_result_to_outcome<T>(result: crate::Result<T>) -> FtOutcome<T> {
    Outcome::from(result)
}

// ---------------------------------------------------------------------------
// OutcomeExt trait
// ---------------------------------------------------------------------------

/// Extension trait adding ft-specific helpers to `Outcome<T, E>`.
pub trait OutcomeExt<T, E> {
    /// Returns `true` if the outcome represents a successful completion.
    fn succeeded(&self) -> bool;

    /// Returns `true` if the outcome represents a cancellation that should
    /// be treated as a graceful shutdown (User or Shutdown kinds).
    fn is_graceful_cancel(&self) -> bool;

    /// Maps a non-Ok outcome into an `Error::Runtime` and returns a
    /// `Result<T, Error>`, discarding structured cancellation info.
    ///
    /// Use at public API boundaries when the caller doesn't understand
    /// Outcome semantics.
    fn into_ft_result(self) -> crate::Result<T>
    where
        E: std::fmt::Display;

    /// Log non-Ok outcomes at appropriate tracing levels, then return self.
    fn trace_non_ok(self, context: &str) -> Self;
}

impl<T, E: std::fmt::Display> OutcomeExt<T, E> for Outcome<T, E> {
    fn succeeded(&self) -> bool {
        self.is_ok()
    }

    fn is_graceful_cancel(&self) -> bool {
        match self {
            Outcome::Cancelled(reason) => {
                matches!(reason.kind, CancelKind::User | CancelKind::Shutdown)
            }
            _ => false,
        }
    }

    fn into_ft_result(self) -> crate::Result<T>
    where
        E: std::fmt::Display,
    {
        match self {
            Outcome::Ok(v) => Ok(v),
            Outcome::Err(e) => Err(Error::Runtime(format!("{e}"))),
            Outcome::Cancelled(r) => Err(Error::Cancelled(format!("{}", r.kind))),
            Outcome::Panicked(p) => Err(Error::Panicked(p.message().to_string())),
        }
    }

    fn trace_non_ok(self, context: &str) -> Self {
        match &self {
            Outcome::Ok(_) => {}
            Outcome::Err(e) => {
                tracing::warn!(context, error = %e, "outcome error");
            }
            Outcome::Cancelled(r) => {
                tracing::info!(context, kind = %r.kind, "outcome cancelled");
            }
            Outcome::Panicked(p) => {
                tracing::error!(context, message = p.message(), "outcome panicked");
            }
        }
        self
    }
}

// ---------------------------------------------------------------------------
// ResultExt trait — Result → Outcome lifting
// ---------------------------------------------------------------------------

/// Extension trait for lifting `crate::Result<T>` into `Outcome<T, crate::Error>`.
pub trait ResultExt<T> {
    /// Lift this `Result` into an `Outcome`, mapping `Ok` → `Outcome::Ok`
    /// and `Err` → `Outcome::Err`.
    fn into_outcome(self) -> FtOutcome<T>;
}

impl<T> ResultExt<T> for crate::Result<T> {
    fn into_outcome(self) -> FtOutcome<T> {
        Outcome::from(self)
    }
}

// ---------------------------------------------------------------------------
// Severity helpers
// ---------------------------------------------------------------------------

/// Map an asupersync `Severity` to a tracing log level.
#[must_use]
pub fn severity_to_log_level(severity: Severity) -> tracing::Level {
    match severity {
        Severity::Ok => tracing::Level::DEBUG,
        Severity::Err => tracing::Level::WARN,
        Severity::Cancelled => tracing::Level::INFO,
        Severity::Panicked => tracing::Level::ERROR,
    }
}

// ---------------------------------------------------------------------------
// CancelReason construction helpers
// ---------------------------------------------------------------------------

/// Create a `CancelReason` for user-initiated cancellation.
///
/// Uses a zero region/task ID and current-ish timestamp. Suitable for
/// adapter-layer use where we don't have a real Cx.
#[must_use]
pub fn cancel_user(message: &'static str) -> CancelReason {
    CancelReason {
        kind: CancelKind::User,
        origin_region: asupersync::RegionId::new_ephemeral(),
        origin_task: None,
        timestamp: asupersync::Time::ZERO,
        message: Some(message),
        cause: None,
        truncated: false,
        truncated_at_depth: None,
    }
}

/// Create a `CancelReason` for timeout.
#[must_use]
pub fn cancel_timeout(message: &'static str) -> CancelReason {
    CancelReason {
        kind: CancelKind::Timeout,
        origin_region: asupersync::RegionId::new_ephemeral(),
        origin_task: None,
        timestamp: asupersync::Time::ZERO,
        message: Some(message),
        cause: None,
        truncated: false,
        truncated_at_depth: None,
    }
}

/// Create a `CancelReason` for shutdown.
#[must_use]
pub fn cancel_shutdown(message: &'static str) -> CancelReason {
    CancelReason {
        kind: CancelKind::Shutdown,
        origin_region: asupersync::RegionId::new_ephemeral(),
        origin_task: None,
        timestamp: asupersync::Time::ZERO,
        message: Some(message),
        cause: None,
        truncated: false,
        truncated_at_depth: None,
    }
}

// ---------------------------------------------------------------------------
// Example migration pattern
// ---------------------------------------------------------------------------

/// Demonstrates the recommended pattern for migrating an existing async
/// function from `Result<T, Error>` to `Outcome<T, Error>`.
///
/// # Before (tokio + Result)
///
/// ```ignore
/// async fn fetch_pane_text(pane_id: u64) -> crate::Result<String> {
///     let text = wezterm_client.get_text(pane_id).await?;
///     if text.is_empty() {
///         return Err(Error::Runtime("empty pane".into()));
///     }
///     Ok(text)
/// }
/// ```
///
/// # After (asupersync + Outcome, internal)
///
/// ```ignore
/// async fn fetch_pane_text_internal(pane_id: u64) -> FtOutcome<String> {
///     let result = wezterm_client.get_text(pane_id).await;
///     let text = try_outcome!(ft_result_to_outcome(result));
///     if text.is_empty() {
///         return Outcome::err(Error::Runtime("empty pane".into()));
///     }
///     Outcome::ok(text)
/// }
///
/// // Public API wrapper
/// pub async fn fetch_pane_text(pane_id: u64) -> crate::Result<String> {
///     ft_outcome_to_result(fetch_pane_text_internal(pane_id).await)
/// }
/// ```
///
/// The key pattern:
/// 1. Internal functions return `FtOutcome<T>`.
/// 2. Use `try_outcome!` instead of `?`.
/// 3. Use `ft_result_to_outcome` to lift existing Result-returning calls.
/// 4. Public API wrappers call `ft_outcome_to_result` at the boundary.
#[cfg(doc)]
pub struct _MigrationPatternDocOnly;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Helpers for building test CancelReasons --

    fn test_cancel(kind: CancelKind) -> CancelReason {
        CancelReason {
            kind,
            origin_region: asupersync::RegionId::new_ephemeral(),
            origin_task: None,
            timestamp: asupersync::Time::ZERO,
            message: None,
            cause: None,
            truncated: false,
            truncated_at_depth: None,
        }
    }

    // -- FtOutcome alias --

    #[test]
    fn ft_outcome_ok() {
        let o: FtOutcome<i32> = Outcome::ok(42);
        assert!(o.is_ok());
        assert_eq!(o.unwrap(), 42);
    }

    #[test]
    fn ft_outcome_err() {
        let o: FtOutcome<i32> = Outcome::err(Error::Runtime("boom".into()));
        assert!(o.is_err());
    }

    #[test]
    fn ft_outcome_cancelled() {
        let o: FtOutcome<i32> = Outcome::cancelled(test_cancel(CancelKind::User));
        assert!(o.is_cancelled());
    }

    #[test]
    fn ft_outcome_panicked() {
        let o: FtOutcome<i32> = Outcome::panicked(PanicPayload::new("oops"));
        assert!(o.is_panicked());
    }

    // -- try_outcome! macro --

    fn try_ok() -> FtOutcome<i32> {
        let v = try_outcome!(Outcome::<i32, Error>::ok(10));
        Outcome::ok(v + 1)
    }

    fn try_err() -> FtOutcome<i32> {
        let _v = try_outcome!(Outcome::<i32, Error>::err(Error::Runtime("fail".into())));
        Outcome::ok(999) // unreachable
    }

    fn try_cancelled() -> FtOutcome<i32> {
        let _v = try_outcome!(Outcome::<i32, Error>::cancelled(test_cancel(
            CancelKind::Timeout
        )));
        Outcome::ok(999) // unreachable
    }

    #[test]
    fn try_outcome_propagates_ok() {
        assert_eq!(try_ok().unwrap(), 11);
    }

    #[test]
    fn try_outcome_propagates_err() {
        assert!(try_err().is_err());
    }

    #[test]
    fn try_outcome_propagates_cancelled() {
        assert!(try_cancelled().is_cancelled());
    }

    // -- outcome_into_result --

    #[test]
    fn outcome_ok_to_result() {
        let r = outcome_into_result(
            Outcome::<_, String>::ok(42),
            |_| "c".to_string(),
            |_| "p".to_string(),
        );
        assert_eq!(r, Ok(42));
    }

    #[test]
    fn outcome_err_to_result() {
        let r = outcome_into_result(Outcome::<i32, _>::err("error"), |_| "c", |_| "p");
        assert_eq!(r, Err("error"));
    }

    #[test]
    fn outcome_cancelled_to_result() {
        let r = outcome_into_result(
            Outcome::<i32, String>::cancelled(test_cancel(CancelKind::User)),
            |reason| format!("cancelled: {}", reason.kind),
            |_| "p".to_string(),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("cancelled"));
    }

    #[test]
    fn outcome_panicked_to_result() {
        let r = outcome_into_result(
            Outcome::<i32, String>::panicked(PanicPayload::new("boom")),
            |_| "c".to_string(),
            |p| format!("panic: {}", p.message()),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("panic: boom"));
    }

    // -- ft_outcome_to_result --

    #[test]
    fn ft_outcome_to_result_ok() {
        let r = ft_outcome_to_result(Outcome::ok(42));
        assert_eq!(r.unwrap(), 42);
    }

    #[test]
    fn ft_outcome_to_result_err() {
        let r = ft_outcome_to_result::<i32>(Outcome::err(Error::Runtime("bad".into())));
        assert!(r.is_err());
    }

    #[test]
    fn ft_outcome_to_result_cancelled() {
        let r = ft_outcome_to_result::<i32>(Outcome::cancelled(test_cancel(CancelKind::Shutdown)));
        let err = r.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("cancelled"), "got: {msg}");
        // Verify it's the Cancelled variant, not Runtime
        assert!(matches!(err, Error::Cancelled(_)));
    }

    #[test]
    fn ft_outcome_to_result_panicked() {
        let r = ft_outcome_to_result::<i32>(Outcome::panicked(PanicPayload::new("kaboom")));
        let err = r.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("kaboom"), "got: {msg}");
        // Verify it's the Panicked variant, not Runtime
        assert!(matches!(err, Error::Panicked(_)));
    }

    // -- result_to_outcome --

    #[test]
    fn result_ok_to_outcome() {
        let o = result_to_outcome::<i32, String>(Ok(7));
        assert!(o.is_ok());
        assert_eq!(o.unwrap(), 7);
    }

    #[test]
    fn result_err_to_outcome() {
        let o = result_to_outcome::<i32, String>(Err("err".to_string()));
        assert!(o.is_err());
    }

    // -- OutcomeExt trait --

    #[test]
    fn succeeded_true_for_ok() {
        let o = Outcome::<_, String>::ok(1);
        assert!(o.succeeded());
    }

    #[test]
    fn succeeded_false_for_err() {
        let o = Outcome::<i32, _>::err("e".to_string());
        assert!(!o.succeeded());
    }

    #[test]
    fn graceful_cancel_user() {
        let o = Outcome::<i32, String>::cancelled(test_cancel(CancelKind::User));
        assert!(o.is_graceful_cancel());
    }

    #[test]
    fn graceful_cancel_shutdown() {
        let o = Outcome::<i32, String>::cancelled(test_cancel(CancelKind::Shutdown));
        assert!(o.is_graceful_cancel());
    }

    #[test]
    fn not_graceful_cancel_timeout() {
        let o = Outcome::<i32, String>::cancelled(test_cancel(CancelKind::Timeout));
        assert!(!o.is_graceful_cancel());
    }

    #[test]
    fn not_graceful_cancel_for_ok() {
        let o = Outcome::<i32, String>::ok(1);
        assert!(!o.is_graceful_cancel());
    }

    #[test]
    fn into_ft_result_ok() {
        let o = Outcome::<_, String>::ok(42);
        let r = o.into_ft_result();
        assert_eq!(r.unwrap(), 42);
    }

    #[test]
    fn into_ft_result_err() {
        let o = Outcome::<i32, _>::err("boom".to_string());
        let r = o.into_ft_result();
        assert!(r.is_err());
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("boom"));
    }

    #[test]
    fn into_ft_result_cancelled() {
        let o = Outcome::<i32, String>::cancelled(test_cancel(CancelKind::Timeout));
        let r = o.into_ft_result();
        let err = r.unwrap_err();
        assert!(matches!(err, Error::Cancelled(_)));
    }

    #[test]
    fn into_ft_result_panicked() {
        let o = Outcome::<i32, String>::panicked(PanicPayload::new("oops"));
        let r = o.into_ft_result();
        let err = r.unwrap_err();
        assert!(matches!(err, Error::Panicked(_)));
        let msg = format!("{err}");
        assert!(msg.contains("oops"));
    }

    // -- ResultExt --

    #[test]
    fn result_ext_ok_into_outcome() {
        let r: crate::Result<i32> = Ok(42);
        let o = r.into_outcome();
        assert!(o.is_ok());
        assert_eq!(o.unwrap(), 42);
    }

    #[test]
    fn result_ext_err_into_outcome() {
        let r: crate::Result<i32> = Err(Error::Runtime("fail".into()));
        let o = r.into_outcome();
        assert!(o.is_err());
    }

    // -- severity helpers --

    #[test]
    fn severity_log_levels() {
        assert_eq!(severity_to_log_level(Severity::Ok), tracing::Level::DEBUG);
        assert_eq!(severity_to_log_level(Severity::Err), tracing::Level::WARN);
        assert_eq!(
            severity_to_log_level(Severity::Cancelled),
            tracing::Level::INFO
        );
        assert_eq!(
            severity_to_log_level(Severity::Panicked),
            tracing::Level::ERROR
        );
    }

    // -- cancel helpers --

    #[test]
    fn cancel_user_helper() {
        let r = cancel_user("user stopped");
        assert_eq!(r.kind, CancelKind::User);
        assert_eq!(r.message, Some("user stopped"));
    }

    #[test]
    fn cancel_timeout_helper() {
        let r = cancel_timeout("timed out");
        assert_eq!(r.kind, CancelKind::Timeout);
    }

    #[test]
    fn cancel_shutdown_helper() {
        let r = cancel_shutdown("shutting down");
        assert_eq!(r.kind, CancelKind::Shutdown);
    }

    // -- Severity lattice verification --

    #[test]
    fn severity_lattice_order() {
        assert!(Severity::Ok < Severity::Err);
        assert!(Severity::Err < Severity::Cancelled);
        assert!(Severity::Cancelled < Severity::Panicked);
    }

    // -- Outcome::join lattice tests --

    #[test]
    fn join_ok_ok_returns_first() {
        let a = Outcome::<_, String>::ok(1);
        let b = Outcome::<_, String>::ok(2);
        let j = a.join(b);
        assert!(j.is_ok());
    }

    #[test]
    fn join_ok_err_returns_err() {
        let a = Outcome::<i32, _>::ok(1);
        let b = Outcome::<i32, _>::err("fail".to_string());
        let j = a.join(b);
        assert!(j.is_err());
    }

    #[test]
    fn join_err_cancelled_returns_cancelled() {
        let a = Outcome::<i32, String>::err("e".to_string());
        let b = Outcome::<i32, String>::cancelled(test_cancel(CancelKind::User));
        let j = a.join(b);
        assert!(j.is_cancelled());
    }

    #[test]
    fn join_cancelled_panicked_returns_panicked() {
        let a = Outcome::<i32, String>::cancelled(test_cancel(CancelKind::User));
        let b = Outcome::<i32, String>::panicked(PanicPayload::new("boom"));
        let j = a.join(b);
        assert!(j.is_panicked());
    }

    // -- Roundtrip tests --

    #[test]
    fn result_outcome_roundtrip_ok() {
        let original: Result<i32, String> = Ok(42);
        let outcome = result_to_outcome(original);
        let result = outcome_into_result(outcome, |_| "c".to_string(), |_| "p".to_string());
        assert_eq!(result, Ok(42));
    }

    #[test]
    fn result_outcome_roundtrip_err() {
        let original: Result<i32, String> = Err("e".to_string());
        let outcome = result_to_outcome(original);
        let result = outcome_into_result(outcome, |_| "c".to_string(), |_| "p".to_string());
        assert_eq!(result, Err("e".to_string()));
    }
}

// ---------------------------------------------------------------------------
// Proptest — Outcome composition laws
// ---------------------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    // -- Arbitrary Outcome strategy --

    fn arb_cancel_kind() -> impl Strategy<Value = CancelKind> {
        prop_oneof![
            Just(CancelKind::User),
            Just(CancelKind::Timeout),
            Just(CancelKind::Deadline),
            Just(CancelKind::FailFast),
            Just(CancelKind::RaceLost),
            Just(CancelKind::Shutdown),
            Just(CancelKind::ParentCancelled),
            Just(CancelKind::LinkedExit),
        ]
    }

    fn arb_cancel_reason() -> impl Strategy<Value = CancelReason> {
        arb_cancel_kind().prop_map(|kind| CancelReason {
            kind,
            origin_region: asupersync::RegionId::new_ephemeral(),
            origin_task: None,
            timestamp: asupersync::Time::ZERO,
            message: None,
            cause: None,
            truncated: false,
            truncated_at_depth: None,
        })
    }

    fn arb_outcome() -> impl Strategy<Value = Outcome<i32, String>> {
        prop_oneof![
            any::<i32>().prop_map(Outcome::ok),
            "[a-z]{1,10}".prop_map(|s| Outcome::err(s)),
            arb_cancel_reason().prop_map(Outcome::cancelled),
            "[a-z]{1,10}".prop_map(|s| Outcome::panicked(PanicPayload::new(s))),
        ]
    }

    // -- map identity law: outcome.map(|x| x) preserves variant --

    proptest! {
        #[test]
        fn map_identity_preserves_variant(v in any::<i32>()) {
            let o = Outcome::<_, String>::ok(v);
            let mapped = o.map(|x| x);
            prop_assert_eq!(mapped.unwrap(), v);
        }

        #[test]
        fn map_identity_preserves_err(s in "[a-z]{1,10}") {
            let o = Outcome::<i32, _>::err(s.clone());
            let mapped = o.map(|x: i32| x);
            prop_assert!(mapped.is_err());
        }

        #[test]
        fn map_identity_preserves_cancelled(kind in arb_cancel_kind()) {
            let reason = CancelReason {
                kind,
                origin_region: asupersync::RegionId::new_ephemeral(),
                origin_task: None,
                timestamp: asupersync::Time::ZERO,
                message: None,
                cause: None,
                truncated: false,
                truncated_at_depth: None,
            };
            let o = Outcome::<i32, String>::cancelled(reason);
            let mapped = o.map(|x: i32| x);
            prop_assert!(mapped.is_cancelled());
        }
    }

    // -- map composition law: outcome.map(f).map(g) == outcome.map(|x| g(f(x))) --

    proptest! {
        #[test]
        fn map_composition(v in any::<i32>()) {
            let f = |x: i32| x.wrapping_add(1);
            let g = |x: i32| x.wrapping_mul(2);

            let o1 = Outcome::<_, String>::ok(v);
            let o2 = Outcome::<_, String>::ok(v);

            let chained = o1.map(f).map(g).unwrap();
            let composed = o2.map(|x| g(f(x))).unwrap();
            prop_assert_eq!(chained, composed);
        }
    }

    // -- and_then left identity: Outcome::ok(v).and_then(f) == f(v) --

    proptest! {
        #[test]
        fn and_then_left_identity(v in any::<i32>()) {
            let f = |x: i32| Outcome::<_, String>::ok(x.wrapping_add(10));
            let result = Outcome::<_, String>::ok(v).and_then(f);
            let direct = f(v);
            // Both should be Ok with same value
            prop_assert_eq!(result.unwrap(), direct.unwrap());
        }
    }

    // -- and_then right identity: outcome.and_then(Outcome::ok) preserves outcome --

    proptest! {
        #[test]
        fn and_then_right_identity_ok(v in any::<i32>()) {
            let o = Outcome::<_, String>::ok(v);
            let result = o.and_then(Outcome::ok);
            prop_assert_eq!(result.unwrap(), v);
        }
    }

    // -- join severity lattice: join(a, b).severity() == max(a.severity(), b.severity()) --

    proptest! {
        #[test]
        fn join_respects_severity_lattice(
            a in arb_outcome(),
            b in arb_outcome(),
        ) {
            let sev_a = a.severity();
            let sev_b = b.severity();
            let joined = a.join(b);
            let expected = std::cmp::max(sev_a, sev_b);
            prop_assert_eq!(joined.severity(), expected);
        }
    }

    // -- join idempotent: outcome.join(same_severity_outcome) has same severity --

    proptest! {
        #[test]
        fn join_idempotent_severity(o in arb_outcome()) {
            let sev = o.severity();
            // Clone by reconstructing same variant
            let same: Outcome<i32, String> = match sev {
                Severity::Ok => Outcome::ok(0),
                Severity::Err => Outcome::err("e".to_string()),
                Severity::Cancelled => Outcome::cancelled(CancelReason {
                    kind: CancelKind::User,
                    origin_region: asupersync::RegionId::new_ephemeral(),
                    origin_task: None,
                    timestamp: asupersync::Time::ZERO,
                    message: None,
                    cause: None,
                    truncated: false,
                    truncated_at_depth: None,
                }),
                Severity::Panicked => Outcome::panicked(PanicPayload::new("p")),
            };
            let joined_sev = o.join(same).severity();
            prop_assert_eq!(joined_sev, sev);
        }
    }

    // -- Roundtrip: Result → Outcome → Result preserves Ok/Err --

    proptest! {
        #[test]
        fn roundtrip_result_ok(v in any::<i32>()) {
            let result: Result<i32, String> = Ok(v);
            let outcome = result_to_outcome(result);
            let back = outcome_into_result(outcome, |_| "c".to_string(), |_| "p".to_string());
            prop_assert_eq!(back, Ok(v));
        }

        #[test]
        fn roundtrip_result_err(s in "[a-z]{1,10}") {
            let result: Result<i32, String> = Err(s.clone());
            let outcome = result_to_outcome(result);
            let back = outcome_into_result(outcome, |_| "c".to_string(), |_| "p".to_string());
            prop_assert_eq!(back, Err(s));
        }
    }

    // -- Severity ordering is total --

    proptest! {
        #[test]
        fn severity_total_order(
            a in arb_outcome(),
            b in arb_outcome(),
        ) {
            let sa = a.severity();
            let sb = b.severity();
            // Total order: exactly one of <, ==, > holds
            prop_assert!(sa <= sb || sa >= sb);
        }
    }
}
