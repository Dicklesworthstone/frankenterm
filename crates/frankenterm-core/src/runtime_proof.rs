//! Type-level proof that an async primitive comes from `runtime_async`.
//!
//! Defined under bead `ft-i2eni.1` (BR-RC-DOCTRINE.G1.1). See
//! [`docs/runtime/runtime-proof-trait.md`](../../../../docs/runtime/runtime-proof-trait.md)
//! for the doctrine; this is the actual seal.
//!
//! # Why
//!
//! `AGENTS.md` states: "direct `tokio` usage is forbidden." Source guards and
//! `cargo deny` enforce that rule externally, but neither makes a forbidden
//! type unrepresentable in Rust's type system. The `RuntimeProof` trait adds a
//! type-level gate: a function that bounds a generic on `T: RuntimeProof`
//! cannot accept a raw tokio primitive as that proof witness, because the
//! supertrait `sealed::Sealed` lives in a private module that external crates
//! cannot name.
//!
//! Adding a new project-owned runtime wrapper requires also adding it to the
//! sealed-impl list here. Forgetting to do that is loud (the wrapper stops
//! being usable in any sealed-bound API) instead of silent. A direct foreign
//! re-export cannot be sealed here because Rust's orphan rules forbid that
//! implementation; such a surface must first be wrapped in a local type.
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
//! use frankenterm_core::runtime_async::{LockAcquireError, Mutex};
//!
//! pub async fn lock_first<T>(cx: &Cx, m: &Mutex<T>) -> Result<(), LockAcquireError> {
//!     // Mutex implements RuntimeProof; passing a tokio::sync::Mutex here
//!     // is a type error from the wrapper's monomorphization.
//!     let _g = m.lock_with_cx(cx).await?;
//!     Ok(())
//! }
//! ```
//!
//! # Invariant test
//!
//! The function below is the canary the bridge plan asks for: it accepts
//! anything that implements `RuntimeProof`, and is impossible to call with
//! a raw tokio primitive. The `compile_fail` doctest at the bottom of this
//! file mechanically pins rejection of that example; as with every rustdoc
//! `compile_fail` test, it does not assert a particular compiler diagnostic.

mod sealed {
    /// Sealed supertrait. Lives in a private module so that no downstream
    /// crate (including `tokio`) can ever satisfy `RuntimeProof`. The set
    /// of types that implement `Sealed` is enumerated in this file and
    /// nowhere else.
    pub trait Sealed {}
}

/// Witness that a value's outer type belongs to `runtime_async`'s explicitly
/// enumerated asupersync-backed surface. This nominal seal is intentionally
/// non-recursive: it does not inspect a wrapper's generic payload types.
/// Cannot be implemented outside this module — `sealed::Sealed` is private.
pub trait RuntimeProof: sealed::Sealed {}

/// Compile-time canary. Accepts only types that satisfy [`RuntimeProof`].
///
/// The seal is operational: any attempt to pass a tokio (or other foreign)
/// async primitive here is a type error. Used in tests and as the anchor
/// for the completed per-API adoption ratchet tracked by `ft-3kv6e`.
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
// the whole point. The compile-fail canary, positive unit tests, and synced
// soundness-model inventory make acceptance-set drift visible.
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

impl<T> sealed::Sealed for runtime_async::mpsc::Sender<T> {}
impl<T> RuntimeProof for runtime_async::mpsc::Sender<T> {}

impl<T> sealed::Sealed for runtime_async::mpsc::WeakSender<T> {}
impl<T> RuntimeProof for runtime_async::mpsc::WeakSender<T> {}

impl<T> sealed::Sealed for runtime_async::mpsc::Receiver<T> {}
impl<T> RuntimeProof for runtime_async::mpsc::Receiver<T> {}

impl<T> sealed::Sealed for runtime_async::mpsc::Reserve<'_, T> {}
impl<T> RuntimeProof for runtime_async::mpsc::Reserve<'_, T> {}

impl<T> sealed::Sealed for runtime_async::mpsc::SendPermit<'_, T> {}
impl<T> RuntimeProof for runtime_async::mpsc::SendPermit<'_, T> {}

impl<T, Caps> sealed::Sealed for runtime_async::mpsc::Recv<'_, T, Caps> {}
impl<T, Caps> RuntimeProof for runtime_async::mpsc::Recv<'_, T, Caps> {}

impl<T, Caps> sealed::Sealed for runtime_async::mpsc::RecvMany<'_, T, Caps> {}
impl<T, Caps> RuntimeProof for runtime_async::mpsc::RecvMany<'_, T, Caps> {}

impl<T> sealed::Sealed for runtime_async::watch::Sender<T> {}
impl<T> RuntimeProof for runtime_async::watch::Sender<T> {}

impl<T> sealed::Sealed for runtime_async::watch::Receiver<T> {}
impl<T> RuntimeProof for runtime_async::watch::Receiver<T> {}

impl<T, Caps> sealed::Sealed for runtime_async::watch::ChangedFuture<'_, '_, T, Caps> {}
impl<T, Caps> RuntimeProof for runtime_async::watch::ChangedFuture<'_, '_, T, Caps> {}

impl<T: Clone> sealed::Sealed for runtime_async::broadcast::Sender<T> {}
impl<T: Clone> RuntimeProof for runtime_async::broadcast::Sender<T> {}

impl<T: Clone> sealed::Sealed for runtime_async::broadcast::Receiver<T> {}
impl<T: Clone> RuntimeProof for runtime_async::broadcast::Receiver<T> {}

impl<T> sealed::Sealed for runtime_async::broadcast::Recv<'_, T> {}
impl<T> RuntimeProof for runtime_async::broadcast::Recv<'_, T> {}

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

// `notify::Notify` remains a direct asupersync re-export, so the orphan rules
// prevent sealing it here. MPSC, watch, broadcast, and oneshot publish local
// wrappers and therefore receive direct seal coverage above. Non-waiting
// foreign error, telemetry, and borrowed-value types do not retain task
// wakers and are intentionally outside this async-primitive proof inventory.

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

    /// Type-level seal guard: compiles iff `T: RuntimeProof`. Needs no value,
    /// so it covers sealed types whose construction would otherwise require a
    /// running runtime (`JoinHandle`/`JoinSet`/`Runtime`).
    fn assert_type_impls_proof<T: RuntimeProof>() {}

    #[test]
    fn task_and_runtime_handles_impl_runtime_proof() {
        // JoinHandle / JoinSet / Runtime were previously untested — only
        // Mutex/RwLock/Semaphore/broadcast/oneshot/Cx had assertions. Pin
        // their RuntimeProof membership at the type level so dropping any of
        // these impls fails the build loudly.
        assert_type_impls_proof::<runtime_async::task::JoinHandle<()>>();
        assert_type_impls_proof::<runtime_async::task::JoinSet<()>>();
        assert_type_impls_proof::<runtime_async::Runtime>();
    }

    #[test]
    fn channel_operation_wrappers_impl_runtime_proof() {
        assert_type_impls_proof::<runtime_async::mpsc::Sender<u8>>();
        assert_type_impls_proof::<runtime_async::mpsc::WeakSender<u8>>();
        assert_type_impls_proof::<runtime_async::mpsc::Receiver<u8>>();
        assert_type_impls_proof::<runtime_async::mpsc::Reserve<'static, u8>>();
        assert_type_impls_proof::<runtime_async::mpsc::SendPermit<'static, u8>>();
        assert_type_impls_proof::<runtime_async::mpsc::Recv<'static, u8>>();
        assert_type_impls_proof::<runtime_async::mpsc::RecvMany<'static, u8>>();
        assert_type_impls_proof::<runtime_async::watch::Sender<u8>>();
        assert_type_impls_proof::<runtime_async::watch::Receiver<u8>>();
        assert_type_impls_proof::<runtime_async::watch::ChangedFuture<'static, 'static, u8>>();
        assert_type_impls_proof::<runtime_async::broadcast::Recv<'static, u8>>();
    }

    // The compile-fail doctest demonstrating that tokio::sync::Mutex is
    // rejected by the seal lives on `assert_runtime_proof` itself (above);
    // doctests on private items are not picked up by rustdoc.
}
