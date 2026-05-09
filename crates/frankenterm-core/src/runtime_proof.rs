//! Type-level proof that an async primitive comes from `runtime_async`.
//!
//! Defined under bead `ft-i2eni.1` (BR-RC-DOCTRINE.G1.1). See
//! [`docs/runtime/runtime-proof-trait.md`](../../../../docs/runtime/runtime-proof-trait.md)
//! for the doctrine; this is the actual seal.
//!
//! # Why
//!
//! `AGENTS.md` states: "direct `tokio` usage is forbidden." Today that rule
//! is enforced by grep guards and `cargo deny`. Both are *runtime* checks —
//! a clever import path or a vendored re-export can sneak past them. The
//! `RuntimeProof` trait closes the gap at the *type* level: any function
//! that bounds a generic on `T: RuntimeProof` cannot accept a tokio type,
//! because the supertrait `sealed::Sealed` lives in a private module that
//! external crates cannot name.
//!
//! Adding new tokio re-exports to runtime_async would require also adding
//! them to the sealed-impl list here. Forgetting to do that is loud (the
//! type stops being usable in any sealed-bound API) instead of silent.
//!
//! # How to use
//!
//! Generic API:
//! ```ignore
//! use frankenterm_core::runtime_proof::RuntimeProof;
//!
//! pub async fn run_under_seal<M: RuntimeProof>(_proof: &M) {
//!     // body uses runtime_async primitives
//! }
//! ```
//!
//! With a `&Cx` thread-through (recommended, since `&Cx` already gates most
//! of core's async surface):
//! ```ignore
//! use frankenterm_core::cx::Cx;
//! use frankenterm_core::runtime_async::Mutex;
//!
//! pub async fn lock_first<T>(cx: &Cx, m: &Mutex<T>) {
//!     // Mutex implements RuntimeProof; passing a tokio::sync::Mutex here
//!     // is a type error from the wrapper's monomorphization.
//!     let _g = m.lock_with_cx(cx).await;
//! }
//! ```
//!
//! # Invariant test
//!
//! The function below is the canary the bridge plan asks for: it accepts
//! anything that implements `RuntimeProof`, and is impossible to call with
//! a tokio type. The `compile_fail` doctest at the bottom of this file
//! proves the negative case mechanically.

mod sealed {
    /// Sealed supertrait. Lives in a private module so that no downstream
    /// crate (including `tokio`) can ever satisfy `RuntimeProof`. The set
    /// of types that implement `Sealed` is enumerated in this file and
    /// nowhere else.
    pub trait Sealed {}
}

/// Witness that a value comes from `runtime_async`'s asupersync-backed
/// surface. Cannot be implemented outside this module — `sealed::Sealed`
/// is private.
pub trait RuntimeProof: sealed::Sealed {}

/// Compile-time canary. Accepts only types that satisfy [`RuntimeProof`].
///
/// The seal is operational: any attempt to pass a tokio (or other foreign)
/// async primitive here is a type error. Used in tests and as the anchor
/// for the wider per-API adoption sweep tracked by ft-i2eni.1's follow-on.
///
/// # Examples
///
/// Accepts a runtime_async wrapper:
///
/// ```
/// use frankenterm_core::runtime_async::Mutex;
/// use frankenterm_core::runtime_proof::assert_runtime_proof;
///
/// let m: Mutex<i32> = Mutex::new(0);
/// assert_runtime_proof(&m);
/// ```
///
/// Rejects `tokio::sync::Mutex` — the canonical "tokio leakage in core"
/// regression. The `sealed::Sealed` supertrait is private to this crate,
/// so no foreign type can ever satisfy [`RuntimeProof`]:
///
/// ```compile_fail
/// use frankenterm_core::runtime_proof::assert_runtime_proof;
///
/// let m = tokio::sync::Mutex::new(0_i32);
/// assert_runtime_proof(&m);
/// ```
#[inline]
pub fn assert_runtime_proof<T: RuntimeProof + ?Sized>(_value: &T) {}

// ─────────────────────────────────────────────────────────────────────────
// Sealed implementations.
//
// Each impl below is paired with the corresponding wrapper type in
// runtime_async.rs. Adding a new wrapper REQUIRES adding it here — that's
// the whole point. The compile_fail doctest at the bottom of the file plus
// the unit tests below ensure the invariant survives refactors.
// ─────────────────────────────────────────────────────────────────────────

use crate::cx::Cx;
use crate::runtime_async;

// Local newtype wrappers in runtime_async — sealed directly. The wrappers
// themselves require `Sized` for the inner type, so the seal does too.
impl<T> sealed::Sealed for runtime_async::Mutex<T> {}
impl<T> RuntimeProof for runtime_async::Mutex<T> {}

impl<T> sealed::Sealed for runtime_async::RwLock<T> {}
impl<T> RuntimeProof for runtime_async::RwLock<T> {}

impl sealed::Sealed for runtime_async::Semaphore {}
impl RuntimeProof for runtime_async::Semaphore {}

impl<T: Clone> sealed::Sealed for runtime_async::broadcast::Sender<T> {}
impl<T: Clone> RuntimeProof for runtime_async::broadcast::Sender<T> {}

impl<T: Clone> sealed::Sealed for runtime_async::broadcast::Receiver<T> {}
impl<T: Clone> RuntimeProof for runtime_async::broadcast::Receiver<T> {}

impl<T> sealed::Sealed for runtime_async::oneshot::Sender<T> {}
impl<T> RuntimeProof for runtime_async::oneshot::Sender<T> {}

impl<T> sealed::Sealed for runtime_async::oneshot::Receiver<T> {}
impl<T> RuntimeProof for runtime_async::oneshot::Receiver<T> {}

impl<T> sealed::Sealed for runtime_async::task::JoinHandle<T> {}
impl<T> RuntimeProof for runtime_async::task::JoinHandle<T> {}

impl<T> sealed::Sealed for runtime_async::task::JoinSet<T> {}
impl<T> RuntimeProof for runtime_async::task::JoinSet<T> {}

impl sealed::Sealed for runtime_async::Runtime {}
impl RuntimeProof for runtime_async::Runtime {}

// `Cx` is the canonical "structured async" witness in frankenterm-core.
// Most public async APIs already thread `&Cx`. Sealing `Cx` itself makes
// every such signature transitively a runtime-proof carrier without
// requiring per-API surgery — the bridge plan acceptance permits this:
// "consume `impl RuntimeProof` somewhere in its signature (or thread `&Cx`
// directly which transitively requires it)".
//
// `tokio::sync::Mutex` cannot satisfy `RuntimeProof` because `sealed::Sealed`
// is private. tokio also cannot synthesize a `Cx` value because the type's
// constructors live inside this crate. The seal is structural on both axes.
impl sealed::Sealed for Cx {}
impl RuntimeProof for Cx {}

// mpsc / watch / notify aliases re-export asupersync types directly from the
// foreign crate; orphan rules forbid sealing them here. They are still
// gated transitively when callers thread `&Cx`. A follow-on bead can wrap
// them in local newtypes if direct seal coverage is needed.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::for_request;
    use crate::runtime_async::{Mutex, RwLock, Semaphore, broadcast, oneshot};

    #[test]
    fn mutex_impls_runtime_proof() {
        let m: Mutex<i32> = Mutex::new(0);
        assert_runtime_proof(&m);
    }

    #[test]
    fn rwlock_impls_runtime_proof() {
        let l: RwLock<i32> = RwLock::new(0);
        assert_runtime_proof(&l);
    }

    #[test]
    fn semaphore_impls_runtime_proof() {
        let s = Semaphore::new(1);
        assert_runtime_proof(&s);
    }

    #[test]
    fn broadcast_pair_impls_runtime_proof() {
        let (tx, rx) = broadcast::channel::<u8>(4);
        assert_runtime_proof(&tx);
        assert_runtime_proof(&rx);
    }

    #[test]
    fn oneshot_pair_impls_runtime_proof() {
        let (tx, rx) = oneshot::channel::<u8>();
        assert_runtime_proof(&tx);
        assert_runtime_proof(&rx);
    }

    #[test]
    fn cx_impls_runtime_proof() {
        let cx = for_request();
        assert_runtime_proof(&cx);
    }

    /// Generic API that consumes `impl RuntimeProof`. Used to anchor the
    /// adoption pattern referenced in `docs/runtime/runtime-proof-trait.md`.
    fn generic_api_pattern<P: RuntimeProof + ?Sized>(p: &P) {
        assert_runtime_proof(p);
    }

    #[test]
    fn generic_api_accepts_sealed_types() {
        let m: Mutex<i32> = Mutex::new(0);
        generic_api_pattern(&m);
        let cx = for_request();
        generic_api_pattern(&cx);
    }

    // The compile-fail doctest demonstrating that tokio::sync::Mutex is
    // rejected by the seal lives on `assert_runtime_proof` itself (above);
    // doctests on private items are not picked up by rustdoc.
}
