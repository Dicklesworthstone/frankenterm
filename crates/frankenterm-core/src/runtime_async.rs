//! Asupersync runtime surface — wrappers and ergonomic helpers.
//!
//! This module provides the project's standard async API surface built on
//! asupersync primitives:
//! - sync primitive wrappers (`Mutex`, `RwLock`, `Semaphore`, ...)
//! - channel modules (`mpsc`, `watch`, `broadcast`, `oneshot`)
//! - runtime lifecycle (`RuntimeBuilder`, `Runtime`, `CompatRuntime`)
//! - time helpers (`sleep`, `timeout`, `sleep_with_cx`, `timeout_with_cx`)
//!
//! The dual-runtime Tokio fallback was removed in ft-xbnl0.2.5. Asupersync
//! is now the sole async runtime. The `asupersync-runtime` feature remains an
//! explicit build-profile contract for runtime-specific tests and benchmarks;
//! it no longer selects between competing production runtimes.

use std::future::Future;
use std::time::Duration;

/// Context-operation failure returned by the canonical async runtime.
///
/// This alias keeps first-party callers on FrankenTerm's owned runtime surface
/// while preserving the structured error value needed for finite
/// classification at transport boundaries.
pub use asupersync::error::Error as ContextError;

/// Stable classification for [`ContextError`].
pub use asupersync::error::ErrorKind as ContextErrorKind;

/// Logical time value used by canonical runtime budget/deadline APIs.
pub use asupersync::types::Time as RuntimeTime;

/// Narrow HTTP/1 client surface used by first-party transports.
///
/// Keep this module deliberately explicit: it is an owned doorway for the
/// concrete client, method, and client error needed by FrankenTerm, not a
/// wildcard re-export of Asupersync's HTTP implementation.
pub mod http {
    pub use asupersync::http::h1::http_client::ClientError;
    pub use asupersync::http::h1::{HttpClient, Method, Response};
}

/// Narrow async-stream trait surface used by first-party streaming APIs.
pub mod stream {
    pub use asupersync::stream::Stream;
}

/// Historical quarantine inventory — kept for audit trail.
///
/// The Tokio runtime builder fallback was removed in ft-xbnl0.2.5.
/// This constant is retained only for surface guard test compatibility.
pub const RAW_TOKIO_RUNTIME_BUILDER_QUARANTINE_V1: &[&str] = &[];

/// Negative-evidence ledger for raw channel values that can retain caller
/// wakers. An empty inventory means every such value published by this module
/// is a project-owned wrapper; only non-waiter error, telemetry, and borrowed
/// watch-value types are re-exported from asupersync.
pub const RAW_ASUPERSYNC_RETAINED_CHANNEL_WAKER_EXPOSURES_V1: &[&str] = &[];

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// Stable project-owned waker passed to async primitives that would otherwise
/// retain and later invoke an executor-provided waker outside FrankenTerm's
/// panic-containment boundary.
///
/// Each published downstream waker is single-consumer: completion, abort, or
/// drop takes it exactly once. The slot is compared and updated under the
/// mutex, but caller-owned wakers are cloned, retired, and invoked only after
/// unlocking; no caller `RawWaker` callback can run while the slot is held.
struct ContainedForwardingWaker {
    downstream: std::sync::Mutex<Option<std::task::Waker>>,
    lock_poisoned_count: &'static std::sync::atomic::AtomicU64,
    callback_panic_count: &'static std::sync::atomic::AtomicU64,
    panic_site: frankenterm_sigpipe::RecoverablePanicSite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ContainedWakerRegistrationError;

impl std::fmt::Display for ContainedWakerRegistrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("caller waker registration failed")
    }
}

impl ContainedForwardingWaker {
    fn new(
        lock_poisoned_count: &'static std::sync::atomic::AtomicU64,
        callback_panic_count: &'static std::sync::atomic::AtomicU64,
        panic_site: frankenterm_sigpipe::RecoverablePanicSite,
    ) -> (Arc<Self>, std::task::Waker) {
        let state = Arc::new(Self {
            downstream: std::sync::Mutex::new(None),
            lock_poisoned_count,
            callback_panic_count,
            panic_site,
        });
        let proxy = std::task::Waker::from(Arc::clone(&state));
        (state, proxy)
    }

    fn lock_downstream_recovering(&self) -> std::sync::MutexGuard<'_, Option<std::task::Waker>> {
        self.downstream.lock().unwrap_or_else(|poison| {
            saturating_increment_counter(self.lock_poisoned_count);
            poison.into_inner()
        })
    }

    /// Publish the current caller waker as one linearized slot update.
    ///
    /// Registration uses a compare/clone/recheck transaction. The initial
    /// comparison avoids steady-state clones; the clone runs outside the lock
    /// inside panic quarantine; and the second comparison linearizes the slot
    /// update against completion or abort that raced with the clone.
    /// `Waker::will_wake` only compares RawWaker identity and invokes no vtable
    /// callback, so it is the sole caller-waker operation performed while
    /// holding the slot lock.
    fn register(
        &self,
        downstream: &std::task::Waker,
    ) -> Result<(), ContainedWakerRegistrationError> {
        self.register_with(downstream, std::task::Waker::clone)
    }

    fn register_with(
        &self,
        downstream: &std::task::Waker,
        clone_downstream: impl FnOnce(&std::task::Waker) -> std::task::Waker,
    ) -> Result<(), ContainedWakerRegistrationError> {
        {
            let slot = self.lock_downstream_recovering();
            if slot
                .as_ref()
                .is_some_and(|current| current.will_wake(downstream))
            {
                return Ok(());
            }
        }

        let candidate = match frankenterm_sigpipe::catch_recoverable(
            self.panic_site,
            std::panic::AssertUnwindSafe(|| clone_downstream(downstream)),
        ) {
            Ok(candidate) => candidate,
            Err(_panic) => {
                saturating_increment_counter(self.callback_panic_count);
                // A previous registration is no longer valid for this poll.
                // Retire it before returning a finite, content-free failure;
                // callers must not poll the primitive without a current waker.
                self.clear();
                return Err(ContainedWakerRegistrationError);
            }
        };

        let mut slot = self.lock_downstream_recovering();
        if slot
            .as_ref()
            .is_some_and(|current| current.will_wake(downstream))
        {
            drop(slot);
            self.dispose(candidate);
            return Ok(());
        }
        let retired = slot.replace(candidate);
        drop(slot);
        if let Some(retired) = retired {
            self.dispose(retired);
        }
        Ok(())
    }

    fn clear(&self) {
        let retired = {
            let mut slot = self.lock_downstream_recovering();
            slot.take()
        };
        if let Some(retired) = retired {
            self.dispose(retired);
        }
    }

    fn forward_one(&self) {
        let downstream = {
            let mut slot = self.lock_downstream_recovering();
            slot.take()
        };
        if let Some(downstream) = downstream {
            if frankenterm_sigpipe::catch_recoverable(
                self.panic_site,
                std::panic::AssertUnwindSafe(|| downstream.wake()),
            )
            .is_err()
            {
                saturating_increment_counter(self.callback_panic_count);
            }
        }
    }

    fn dispose(&self, downstream: std::task::Waker) {
        Self::dispose_at(self.panic_site, self.callback_panic_count, downstream);
    }

    fn dispose_at(
        panic_site: frankenterm_sigpipe::RecoverablePanicSite,
        callback_panic_count: &'static std::sync::atomic::AtomicU64,
        downstream: std::task::Waker,
    ) {
        if frankenterm_sigpipe::catch_recoverable(
            panic_site,
            std::panic::AssertUnwindSafe(|| drop(downstream)),
        )
        .is_err()
        {
            saturating_increment_counter(callback_panic_count);
        }
    }
}

impl std::task::Wake for ContainedForwardingWaker {
    fn wake(self: Arc<Self>) {
        self.forward_one();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.forward_one();
    }
}

impl Drop for ContainedForwardingWaker {
    fn drop(&mut self) {
        // Normal terminal and wrapper-drop paths clear the slot first. This
        // final backstop guarantees that even a future maintenance regression
        // cannot let a residual caller RawWaker drop escape state destruction.
        let panic_site = self.panic_site;
        let lock_poisoned_count = self.lock_poisoned_count;
        let callback_panic_count = self.callback_panic_count;
        let downstream = match self.downstream.get_mut() {
            Ok(slot) => slot.take(),
            Err(poison) => {
                saturating_increment_counter(lock_poisoned_count);
                poison.into_inner().take()
            }
        };
        if let Some(downstream) = downstream {
            Self::dispose_at(panic_site, callback_panic_count, downstream);
        }
    }
}

fn saturating_increment_counter(counter: &std::sync::atomic::AtomicU64) {
    let mut current = counter.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        if current == u64::MAX {
            return;
        }
        match counter.compare_exchange_weak(
            current,
            current + 1,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

/// Reusable stable proxy boundary for a channel receiver. The boundary is
/// created lazily by each wrapper only when an operation can actually retain a
/// task waker, then reused for subsequent waits on that receiver.
struct ContainedWakerBoundary {
    forwarding: Arc<ContainedForwardingWaker>,
    proxy: std::task::Waker,
}

impl ContainedWakerBoundary {
    fn new(
        lock_poisoned_count: &'static std::sync::atomic::AtomicU64,
        callback_panic_count: &'static std::sync::atomic::AtomicU64,
    ) -> Self {
        let (forwarding, proxy) = ContainedForwardingWaker::new(
            lock_poisoned_count,
            callback_panic_count,
            frankenterm_sigpipe::RecoverablePanicSite::CoreChannelWaker,
        );
        Self { forwarding, proxy }
    }

    fn register(
        &self,
        downstream: &std::task::Waker,
    ) -> Result<(), ContainedWakerRegistrationError> {
        self.forwarding.register(downstream)
    }

    fn clear(&self) {
        self.forwarding.clear();
    }

    fn proxy(&self) -> &std::task::Waker {
        &self.proxy
    }
}

/// Poll a channel primitive through a stable trusted proxy. This is the
/// general-purpose path for primitives whose wake can be edge-triggered rather
/// than coupled to durable readiness: the caller registration must exist
/// before the inner primitive receives any waker.
#[inline]
fn poll_with_contained_channel_waker<R>(
    boundary_slot: &mut Option<ContainedWakerBoundary>,
    caller_cx: &std::task::Context<'_>,
    mut poll_inner: impl FnMut(&mut std::task::Context<'_>) -> std::task::Poll<R>,
    new_boundary: impl FnOnce() -> ContainedWakerBoundary,
    registration_failure: impl FnOnce() -> R,
) -> std::task::Poll<R> {
    let boundary = boundary_slot.get_or_insert_with(new_boundary);
    // Publish the current caller before the one inner poll. A wake that races
    // with the compare/clone phase may still consume the preceding valid
    // registration; this poll then observes the resulting state. Any wake
    // after the slot update sees this caller or a later one.
    if boundary.register(caller_cx.waker()).is_err() {
        return std::task::Poll::Ready(registration_failure());
    }
    let mut proxy_cx = std::task::Context::from_waker(boundary.proxy());
    let result = poll_inner(&mut proxy_cx);
    if result.is_ready() {
        boundary.clear();
    }
    result
}

/// Poll a channel primitive without cloning or allocating a caller-waker proxy
/// on an immediately-ready path. This optimization is sound only when every
/// wake is coupled to state that remains observable by the immediate proxy
/// repoll. A pending first probe installs only a trusted no-op waker; the
/// second poll publishes the stable proxy after the caller waker has been
/// quarantined behind it. Re-polls reuse the existing proxy.
#[inline]
fn poll_with_durable_probe_contained_channel_waker<R>(
    boundary_slot: &mut Option<ContainedWakerBoundary>,
    caller_cx: &std::task::Context<'_>,
    mut poll_inner: impl FnMut(&mut std::task::Context<'_>) -> std::task::Poll<R>,
    new_boundary: impl FnOnce() -> ContainedWakerBoundary,
    registration_failure: impl FnOnce() -> R,
) -> std::task::Poll<R> {
    if boundary_slot.is_some() {
        return poll_with_contained_channel_waker(
            boundary_slot,
            caller_cx,
            poll_inner,
            new_boundary,
            registration_failure,
        );
    }

    let mut noop_cx = std::task::Context::from_waker(std::task::Waker::noop());
    let first_poll = poll_inner(&mut noop_cx);
    if first_poll.is_ready() {
        return first_poll;
    }

    // A wake racing with the no-op registration requests a poll that this
    // invocation is already about to perform. The immediate proxy repoll both
    // observes any durable ready state and replaces the temporary no-op before
    // returning Pending, closing the publication window without exposing the
    // caller's waker to the inner channel.
    poll_with_contained_channel_waker(
        boundary_slot,
        caller_cx,
        poll_inner,
        new_boundary,
        registration_failure,
    )
}

/// Clears a forwarding slot before the wrapped pending future is dropped.
/// Declare this guard after that future so Rust's reverse local-drop order
/// retires the downstream waker before the primitive releases its proxy.
struct ClearContainedWakerOnDrop {
    state: Arc<ContainedForwardingWaker>,
}

impl ClearContainedWakerOnDrop {
    fn new(state: Arc<ContainedForwardingWaker>) -> Self {
        Self { state }
    }

    fn register(
        &self,
        downstream: &std::task::Waker,
    ) -> Result<(), ContainedWakerRegistrationError> {
        self.state.register(downstream)
    }

    fn clear(&self) {
        self.state.clear();
    }
}

impl Drop for ClearContainedWakerOnDrop {
    fn drop(&mut self) {
        self.clear();
    }
}

// Thread-local storage for the asupersync `RuntimeHandle`, installed by
// `Runtime::block_on` and consumed by `task::spawn` to provide ambient
// runtime context (analogous to tokio's internal CONTEXT thread-local).
thread_local! {
    static ASUPERSYNC_HANDLE: std::cell::RefCell<Option<asupersync::runtime::RuntimeHandle>> =
        const { std::cell::RefCell::new(None) };
    static ASUPERSYNC_SHUTDOWN_TOKEN: std::cell::RefCell<Option<RuntimeShutdownToken>> =
        const { std::cell::RefCell::new(None) };
}

struct RuntimeShutdownState {
    requested: std::sync::atomic::AtomicBool,
    active_leases: std::sync::Mutex<usize>,
    changed: std::sync::Condvar,
}

static RUNTIME_SHUTDOWN_REQUESTED_TOTAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static RUNTIME_SHUTDOWN_DRAINED_TOTAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static RUNTIME_SHUTDOWN_DRAIN_TIMEOUT_TOTAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Content-free process totals for finite runtime cleanup drains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeShutdownMetrics {
    /// Runtime wrappers that began their one-way shutdown transition.
    pub requested_total: u64,
    /// Shutdown transitions whose cleanup leases settled before the deadline.
    pub drained_total: u64,
    /// Shutdown transitions that reached the finite lease-drain deadline.
    pub drain_timeout_total: u64,
}

/// Snapshot process-local runtime shutdown outcomes without acquiring a lock.
#[must_use]
pub fn runtime_shutdown_metrics() -> RuntimeShutdownMetrics {
    RuntimeShutdownMetrics {
        requested_total: RUNTIME_SHUTDOWN_REQUESTED_TOTAL
            .load(std::sync::atomic::Ordering::Relaxed),
        drained_total: RUNTIME_SHUTDOWN_DRAINED_TOTAL.load(std::sync::atomic::Ordering::Relaxed),
        drain_timeout_total: RUNTIME_SHUTDOWN_DRAIN_TIMEOUT_TOTAL
            .load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Runtime-instance shutdown authority shared with bounded cleanup work.
///
/// This token never owns the runtime. It can therefore be retained by a task
/// or blocking closure without recreating the runtime ownership cycle that the
/// ambient handle adapters must avoid.
#[derive(Clone)]
pub(crate) struct RuntimeShutdownToken {
    state: std::sync::Arc<RuntimeShutdownState>,
}

impl RuntimeShutdownToken {
    fn new() -> Self {
        Self {
            state: std::sync::Arc::new(RuntimeShutdownState {
                requested: std::sync::atomic::AtomicBool::new(false),
                active_leases: std::sync::Mutex::new(0),
                changed: std::sync::Condvar::new(),
            }),
        }
    }

    /// Acquire one cleanup lease while admission remains open.
    #[cfg(any(feature = "session-resume", test))]
    pub(crate) fn try_acquire(&self) -> Option<RuntimeShutdownLease> {
        let mut active = self
            .state
            .active_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_shutdown_requested() {
            return None;
        }
        *active = (*active).checked_add(1)?;
        Some(RuntimeShutdownLease {
            state: Some(std::sync::Arc::clone(&self.state)),
        })
    }

    /// Return whether this runtime has begun its one-way shutdown transition.
    #[cfg(any(feature = "session-resume", test))]
    pub(crate) fn is_shutdown_requested(&self) -> bool {
        self.state
            .requested
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn request_shutdown_and_wait(&self, timeout: Duration) -> bool {
        let started = std::time::Instant::now();
        let mut active = self
            .state
            .active_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.state
            .requested
            .store(true, std::sync::atomic::Ordering::Release);
        self.state.changed.notify_all();

        while *active > 0 {
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return false;
            };
            let (next, wait) = self
                .state
                .changed
                .wait_timeout(active, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active = next;
            if wait.timed_out() && *active > 0 {
                return false;
            }
        }
        true
    }

    /// Begin the one-way shutdown transition without waiting beyond the
    /// current instant. This seam exists only so sibling-module tests can prove
    /// that subsystem admission rejects a live-but-shutting-down runtime before
    /// any work is admitted.
    #[cfg(all(test, feature = "session-resume"))]
    pub(crate) fn request_shutdown_for_test(&self) -> bool {
        self.request_shutdown_and_wait(Duration::ZERO)
    }
}

/// Proof that one admitted cleanup transaction still belongs to a live
/// runtime shutdown drain.
#[cfg(any(feature = "session-resume", test))]
pub(crate) struct RuntimeShutdownLease {
    state: Option<std::sync::Arc<RuntimeShutdownState>>,
}

#[cfg(any(feature = "session-resume", test))]
impl Drop for RuntimeShutdownLease {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let mut active = state
            .active_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = *active;
        *active = previous.saturating_sub(1);
        debug_assert!(previous > 0, "runtime shutdown lease underflow");
        state.changed.notify_all();
    }
}

#[must_use]
pub(crate) fn current_runtime_shutdown_token() -> Option<RuntimeShutdownToken> {
    ASUPERSYNC_SHUTDOWN_TOKEN.with(|cell| cell.borrow().clone())
}

/// Install an asupersync `RuntimeHandle` into thread-local storage for
/// ambient `task::spawn` access and inherited-handle helper paths.
///
/// The `runtime_async::Runtime::block_on` wrapper calls this automatically.
/// Test fixtures using the raw asupersync runtime should call this manually.
pub fn install_runtime_handle(handle: asupersync::runtime::RuntimeHandle) {
    ASUPERSYNC_HANDLE.with(|cell| cell.replace(Some(handle)));
}

/// Per-poll runtime-handle installation guard.
///
/// Spawn adapters can be polled while another runtime handle is already
/// installed on the worker thread. Restoring that prior value on drop keeps a
/// nested or sequential runtime from leaking its spawn authority into sibling
/// work after the poll returns.
pub(crate) struct ScopedRuntimeHandle {
    previous: Option<asupersync::runtime::RuntimeHandle>,
}

pub(crate) struct ScopedRuntimeShutdownToken {
    previous: Option<RuntimeShutdownToken>,
}

impl Drop for ScopedRuntimeShutdownToken {
    fn drop(&mut self) {
        let _ = ASUPERSYNC_SHUTDOWN_TOKEN.try_with(|cell| {
            cell.replace(self.previous.take());
        });
    }
}

impl Drop for ScopedRuntimeHandle {
    fn drop(&mut self) {
        let _ = ASUPERSYNC_HANDLE.try_with(|cell| {
            cell.replace(self.previous.take());
        });
    }
}

#[must_use]
pub(crate) fn install_runtime_handle_scoped(
    handle: asupersync::runtime::RuntimeHandle,
) -> ScopedRuntimeHandle {
    let previous = ASUPERSYNC_HANDLE.with(|cell| cell.replace(Some(handle)));
    ScopedRuntimeHandle { previous }
}

#[must_use]
pub(crate) fn install_runtime_shutdown_token_scoped(
    token: Option<RuntimeShutdownToken>,
) -> ScopedRuntimeShutdownToken {
    let previous = ASUPERSYNC_SHUTDOWN_TOKEN.with(|cell| cell.replace(token));
    ScopedRuntimeShutdownToken { previous }
}

/// Return the currently installed asupersync `RuntimeHandle`, if any.
#[must_use]
pub fn current_runtime_handle() -> Option<asupersync::runtime::RuntimeHandle> {
    ASUPERSYNC_HANDLE.with(|cell| cell.borrow().as_ref().cloned())
}

/// Remove the asupersync `RuntimeHandle` from thread-local storage.
pub fn clear_runtime_handle() {
    ASUPERSYNC_HANDLE.with(|cell| cell.replace(None));
    ASUPERSYNC_SHUTDOWN_TOKEN.with(|cell| cell.replace(None));
}

/// Project-owned error surface for fallible mutex and rwlock acquisition.
///
/// Keeping this type in `runtime_async` prevents callers from depending on
/// asupersync's crate-internal lock error types while retaining the failure
/// distinctions needed for cancellation-safe control flow and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockAcquireError {
    /// A panic occurred while a guard was held.
    Poisoned,
    /// The caller's capability context was explicitly cancelled.
    Cancelled,
    /// The caller's capability-context deadline elapsed.
    DeadlineExceeded,
    /// The caller exhausted its cooperative poll quota.
    PollQuotaExhausted,
    /// The caller exhausted its cost budget.
    CostBudgetExhausted,
    /// The capability checkpoint failed without a stable typed root cause.
    ContextFailure,
    /// The lock's own logical acquisition deadline elapsed.
    TimedOut {
        /// Logical deadline reported by asupersync, in nanoseconds.
        deadline_nanos: u64,
    },
    /// The underlying acquisition future was polled after completion.
    PolledAfterCompletion,
}

impl std::fmt::Display for LockAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Poisoned => write!(f, "lock is poisoned"),
            Self::Cancelled => write!(f, "lock acquisition cancelled"),
            Self::DeadlineExceeded => write!(f, "lock capability deadline exceeded"),
            Self::PollQuotaExhausted => write!(f, "lock capability poll quota exhausted"),
            Self::CostBudgetExhausted => write!(f, "lock capability cost budget exhausted"),
            Self::ContextFailure => write!(f, "lock capability context failed"),
            Self::TimedOut { deadline_nanos } => {
                write!(f, "lock acquisition timed out at {deadline_nanos}ns")
            }
            Self::PolledAfterCompletion => {
                write!(f, "lock acquisition future polled after completion")
            }
        }
    }
}

impl std::error::Error for LockAcquireError {}

fn classify_lock_context_failure(cx: &crate::cx::Cx) -> LockAcquireError {
    use crate::outcome::CancelKind;

    match cx.root_cancel_cause().map(|reason| reason.kind) {
        Some(CancelKind::Deadline | CancelKind::Timeout) => LockAcquireError::DeadlineExceeded,
        Some(CancelKind::PollQuota) => LockAcquireError::PollQuotaExhausted,
        Some(CancelKind::CostBudget) => LockAcquireError::CostBudgetExhausted,
        Some(
            CancelKind::User
            | CancelKind::FailFast
            | CancelKind::RaceLost
            | CancelKind::ParentCancelled
            | CancelKind::ResourceUnavailable
            | CancelKind::Shutdown
            | CancelKind::LinkedExit,
        ) => LockAcquireError::Cancelled,
        None => LockAcquireError::ContextFailure,
    }
}

fn map_mutex_lock_error(
    cx: &crate::cx::Cx,
    error: asupersync::sync::LockError,
) -> LockAcquireError {
    match error {
        asupersync::sync::LockError::Poisoned => LockAcquireError::Poisoned,
        asupersync::sync::LockError::Cancelled => classify_lock_context_failure(cx),
        asupersync::sync::LockError::TimedOut(deadline) => LockAcquireError::TimedOut {
            deadline_nanos: deadline.as_nanos(),
        },
        asupersync::sync::LockError::PolledAfterCompletion => {
            LockAcquireError::PolledAfterCompletion
        }
    }
}

fn map_rwlock_error(cx: &crate::cx::Cx, error: asupersync::sync::RwLockError) -> LockAcquireError {
    match error {
        asupersync::sync::RwLockError::Poisoned => LockAcquireError::Poisoned,
        asupersync::sync::RwLockError::Cancelled => classify_lock_context_failure(cx),
        asupersync::sync::RwLockError::PolledAfterCompletion => {
            LockAcquireError::PolledAfterCompletion
        }
    }
}

#[derive(Debug)]
pub struct Mutex<T> {
    inner: asupersync::sync::Mutex<T>,
}

impl<T> Mutex<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            inner: asupersync::sync::Mutex::new(value),
        }
    }

    pub async fn lock(&self) -> MutexGuard<'_, T> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.lock_with_cx(&cx)
            .await
            .expect("runtime_async mutex lock failed")
    }

    /// Acquire the mutex bound to the caller's asupersync capability
    /// context (ft-xbnl0.2.x Cx-first primitive).
    ///
    /// Preferred over [`lock`](Self::lock) when the call site already
    /// threads `&Cx` through its public API — budget-driven cancellation
    /// and deadline propagation from the outer scope cut the acquire
    /// wait deterministically under `LabRuntime` virtual time instead
    /// of relying on `Cx::current()` thread-local lookup.
    ///
    /// This method never turns cancellation, poisoning, or deadline expiry into
    /// a panic. A cancellation requested while the future is suspended is
    /// observed on its next poll; cancellation does not itself guarantee that a
    /// contended lock waiter is woken, so callers requiring prompt cancellation
    /// must race the acquire against a cancellation signal.
    ///
    /// # Errors
    ///
    /// Returns [`LockAcquireError`] with the exact project-level failure class
    /// reported by the underlying lock acquisition.
    pub async fn lock_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<MutexGuard<'_, T>, LockAcquireError> {
        self.inner
            .lock(cx)
            .await
            .map(|inner| MutexGuard { inner })
            .map_err(|error| map_mutex_lock_error(cx, error))
    }
}

pub struct MutexGuard<'a, T> {
    inner: asupersync::sync::MutexGuard<'a, T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.deref_mut()
    }
}

/// Opt-in mutex for operations that deliberately retain exclusion across an
/// await point in a Send task.
///
/// Unlike the ordinary allocation-free [`Mutex`], this type stores its
/// primitive behind an [`Arc`] so its guard can own the lock. Use it only for
/// suspension-spanning critical sections.
#[derive(Debug)]
pub struct OwnedMutex<T> {
    inner: Arc<asupersync::sync::Mutex<T>>,
}

impl<T> Clone for OwnedMutex<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> OwnedMutex<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            inner: Arc::new(asupersync::sync::Mutex::new(value)),
        }
    }

    pub async fn lock_owned(&self) -> OwnedMutexGuard<T> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.lock_owned_with_cx(&cx)
            .await
            .expect("runtime_async owned mutex lock failed")
    }

    /// Cx-aware form of [`lock_owned`](Self::lock_owned).
    ///
    /// # Errors
    ///
    /// Returns [`LockAcquireError`] with the exact project-level failure class
    /// reported by the underlying owned lock acquisition.
    pub async fn lock_owned_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<OwnedMutexGuard<T>, LockAcquireError> {
        asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&self.inner), cx)
            .await
            .map(|inner| OwnedMutexGuard { inner })
            .map_err(|error| map_mutex_lock_error(cx, error))
    }

    /// Whether a cloned handle, waiter, or live guard references this mutex.
    #[must_use]
    pub fn has_external_owner(&self) -> bool {
        Arc::strong_count(&self.inner) > 1
    }
}

/// Send-capable guard returned by [`OwnedMutex::lock_owned`].
#[must_use = "the owned mutex is released immediately if its guard is not held"]
pub struct OwnedMutexGuard<T> {
    inner: asupersync::sync::OwnedMutexGuard<T>,
}

impl<T> Deref for OwnedMutexGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl<T> DerefMut for OwnedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.deref_mut()
    }
}

#[derive(Debug)]
pub struct RwLock<T> {
    inner: asupersync::sync::RwLock<T>,
}

impl<T> RwLock<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            inner: asupersync::sync::RwLock::new(value),
        }
    }

    #[allow(clippy::future_not_send)] // asupersync RwLock is !Sync by design
    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.read_with_cx(&cx)
            .await
            .expect("runtime_async rwlock read failed")
    }

    /// Acquire a read guard bound to the caller's asupersync capability
    /// context (ft-xbnl0.2.x Cx-first primitive).
    ///
    /// Preferred over [`read`](Self::read) when the call site already threads
    /// `&Cx` through its public API.
    ///
    /// Cancellation requested while a contended read is suspended is observed
    /// on its next poll. Callers requiring cancellation itself to wake the
    /// waiter must race the acquire against a cancellation signal.
    ///
    /// # Errors
    ///
    /// Returns [`LockAcquireError`] instead of panicking when acquisition is
    /// cancelled, poisoned, or otherwise cannot complete.
    #[allow(clippy::future_not_send)] // asupersync RwLock is !Sync by design
    pub async fn read_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<RwLockReadGuard<'_, T>, LockAcquireError> {
        self.inner
            .read(cx)
            .await
            .map(|inner| RwLockReadGuard { inner })
            .map_err(|error| map_rwlock_error(cx, error))
    }

    #[allow(clippy::future_not_send)] // asupersync RwLock is !Sync by design
    pub async fn write(&self) -> RwLockWriteGuard<'_, T> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.write_with_cx(&cx)
            .await
            .expect("runtime_async rwlock write failed")
    }

    /// Acquire a write guard bound to the caller's asupersync capability
    /// context (ft-xbnl0.2.x Cx-first primitive).
    ///
    /// Preferred over [`write`](Self::write) when the call site already
    /// threads `&Cx`.
    ///
    /// Cancellation requested while a contended write is suspended is observed
    /// on its next poll. Callers requiring cancellation itself to wake the
    /// waiter must race the acquire against a cancellation signal.
    ///
    /// # Errors
    ///
    /// Returns [`LockAcquireError`] instead of panicking when acquisition is
    /// cancelled, poisoned, or otherwise cannot complete.
    #[allow(clippy::future_not_send)] // asupersync RwLock is !Sync by design
    pub async fn write_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<RwLockWriteGuard<'_, T>, LockAcquireError> {
        self.inner
            .write(cx)
            .await
            .map(|inner| RwLockWriteGuard { inner })
            .map_err(|error| map_rwlock_error(cx, error))
    }
}

pub struct RwLockReadGuard<'a, T> {
    inner: asupersync::sync::RwLockReadGuard<'a, T>,
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

pub struct RwLockWriteGuard<'a, T> {
    inner: asupersync::sync::RwLockWriteGuard<'a, T>,
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.deref_mut()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryAcquireError {
    NoPermits,
    Closed,
}

impl std::fmt::Display for TryAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPermits => write!(f, "no semaphore permits available"),
            Self::Closed => write!(f, "semaphore closed"),
        }
    }
}

impl std::error::Error for TryAcquireError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireError {
    Closed,
    Cancelled,
    PolledAfterCompletion,
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "semaphore closed"),
            Self::Cancelled => write!(f, "semaphore acquire cancelled"),
            Self::PolledAfterCompletion => {
                write!(f, "semaphore acquire future polled after completion")
            }
        }
    }
}

impl std::error::Error for AcquireError {}

#[derive(Debug)]
pub struct Semaphore {
    inner: Arc<asupersync::sync::Semaphore>,
}

impl Semaphore {
    fn map_acquire_error(err: asupersync::sync::AcquireError) -> AcquireError {
        match err {
            asupersync::sync::AcquireError::Closed => AcquireError::Closed,
            asupersync::sync::AcquireError::Cancelled => AcquireError::Cancelled,
            asupersync::sync::AcquireError::PolledAfterCompletion => {
                AcquireError::PolledAfterCompletion
            }
        }
    }

    #[must_use]
    pub fn new(permits: usize) -> Self {
        Self {
            inner: Arc::new(asupersync::sync::Semaphore::new(permits)),
        }
    }

    #[must_use]
    pub fn available_permits(&self) -> usize {
        self.inner.available_permits()
    }

    pub fn close(&self) {
        self.inner.close();
    }

    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        if self.inner.is_closed() {
            return Err(TryAcquireError::Closed);
        }

        self.inner
            .try_acquire(1)
            .map(|inner| SemaphorePermit { inner })
            .map_err(|_| {
                if self.inner.is_closed() {
                    TryAcquireError::Closed
                } else {
                    TryAcquireError::NoPermits
                }
            })
    }

    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.acquire_with_cx(&cx).await
    }

    /// Acquire a permit under an explicit `&Cx` (ft-xbnl0.2.x Cx-first
    /// primitive). Preferred over [`acquire`](Self::acquire) when the
    /// caller already threads `&Cx` through its public API.
    ///
    /// # Cancellation semantics
    ///
    /// Observes **pre-cancel**: a pre-cancelled cx returns
    /// `Err(AcquireError::Cancelled)` promptly via asupersync's per-poll
    /// `cx.checkpoint()` short-circuit (pinned by
    /// `semaphore_acquire_with_cx_observes_pre_cancel`, ft-xbnl0.2.4
    /// tick 421).
    ///
    /// Does NOT observe **mid-flight cancel**: asupersync's semaphore
    /// acquire does not register a cx-cancel-waker for an already-
    /// suspended acquire (pinned by
    /// `semaphore_acquire_with_cx_mid_flight_cancel_via_select_race_pattern`,
    /// tick 439a). Callers needing mid-flight cancel observability
    /// must wrap in `futures::future::select` against a poll-sleep
    /// watcher (same pattern as `DistributedHttpClient::race_with_cx_cancel`,
    /// tick 387). See `docs/ft-xbnl0-2-4-completion-evidence.md` §2.6.1.
    pub async fn acquire_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.inner
            .acquire(cx, 1)
            .await
            .map(|inner| SemaphorePermit { inner })
            .map_err(Self::map_acquire_error)
    }

    pub fn try_acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        if self.inner.is_closed() {
            return Err(TryAcquireError::Closed);
        }

        asupersync::sync::OwnedSemaphorePermit::try_acquire(self.inner.clone(), 1)
            .map(|inner| OwnedSemaphorePermit { inner })
            .map_err(|_| {
                if self.inner.is_closed() {
                    TryAcquireError::Closed
                } else {
                    TryAcquireError::NoPermits
                }
            })
    }

    pub async fn acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, AcquireError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.acquire_owned_with_cx(&cx).await
    }

    /// Acquire a permit as `OwnedSemaphorePermit` under an explicit
    /// `&Cx` — owned-permit companion to [`acquire_with_cx`]. Used
    /// when the permit needs to cross an await boundary or be moved
    /// into a spawned task.
    ///
    /// # Cancellation semantics
    ///
    /// Same as [`acquire_with_cx`]: observes pre-cancel (pinned by
    /// `semaphore_acquire_owned_with_cx_observes_pre_cancel`, tick
    /// 427); does NOT register a cx-cancel-waker for already-suspended
    /// acquires. The tick-421/439a tests pin the borrow variant; the
    /// tick-427 pre-cancel test pins the owned variant. Callers
    /// needing mid-flight cancel observability should use the same
    /// select-race pattern. See
    /// `docs/ft-xbnl0-2-4-completion-evidence.md` §2.6.1.
    pub async fn acquire_owned_with_cx(
        self: Arc<Self>,
        cx: &crate::cx::Cx,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        asupersync::sync::OwnedSemaphorePermit::acquire(self.inner.clone(), cx, 1)
            .await
            .map(|inner| OwnedSemaphorePermit { inner })
            .map_err(Self::map_acquire_error)
    }
}

pub struct SemaphorePermit<'a> {
    inner: asupersync::sync::SemaphorePermit<'a>,
}

impl SemaphorePermit<'_> {
    #[must_use]
    pub fn count(&self) -> usize {
        self.inner.count()
    }
}

impl std::fmt::Debug for SemaphorePermit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemaphorePermit")
            .field("count", &self.count())
            .finish()
    }
}

#[derive(Debug)]
pub struct OwnedSemaphorePermit {
    inner: asupersync::sync::OwnedSemaphorePermit,
}

impl OwnedSemaphorePermit {
    #[must_use]
    pub fn count(&self) -> usize {
        self.inner.count()
    }
}

/// Project-owned MPSC wrappers for the active runtime.
///
/// Under asupersync the receiver's `recv(cx)` method observes
/// **pre-cancel** on cx via the per-poll `cx.checkpoint()`
/// short-circuit (pinned by `mpsc_recv_with_cx_observes_pre_cancel`,
/// ft-xbnl0.2.4 tick 422) but does NOT register a cx-cancel-waker.
/// An already-suspended recv will NOT wake when `cx.cancel_with(...)`
/// fires afterward (pinned by
/// `mpsc_recv_with_cx_mid_flight_cancel_via_select_race_pattern`,
/// tick 432). Callers needing mid-flight cancel must wrap in
/// `futures::future::select` against a poll-sleep watcher (same
/// pattern as `DistributedHttpClient::race_with_cx_cancel`, tick
/// 387). See `docs/ft-xbnl0-2-4-completion-evidence.md` §2.6.1.
pub mod mpsc {
    use asupersync::channel::mpsc as inner;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub use inner::{MpscTelemetrySnapshot, RecvError, SendError};

    static RETAINED_WAKER_LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    static RETAINED_WAKER_CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    /// Number of recovered forwarding-slot poison events for MPSC waiters.
    #[must_use]
    pub fn retained_waker_lock_poisoned_count() -> u64 {
        RETAINED_WAKER_LOCK_POISONED_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of caller waker clone, wake, or drop panics contained at the
    /// canonical MPSC boundary.
    #[must_use]
    pub fn retained_waker_callback_panic_count() -> u64 {
        RETAINED_WAKER_CALLBACK_PANIC_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn new_waker_boundary() -> super::ContainedWakerBoundary {
        super::ContainedWakerBoundary::new(
            &RETAINED_WAKER_LOCK_POISONED_COUNT,
            &RETAINED_WAKER_CALLBACK_PANIC_COUNT,
        )
    }

    /// Creates a bounded MPSC channel.
    #[must_use]
    pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
        let (sender, receiver) = inner::channel(capacity);
        (
            Sender { inner: sender },
            Receiver {
                inner: receiver,
                retained_waker: None,
            },
        )
    }

    /// Project-owned sending side of a bounded MPSC channel.
    pub struct Sender<T> {
        inner: inner::Sender<T>,
    }

    impl<T> std::fmt::Debug for Sender<T> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("mpsc::Sender")
                .finish_non_exhaustive()
        }
    }

    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
            }
        }
    }

    impl<T> Sender<T> {
        /// Reserves one channel slot. A stable trusted waker proxy is allocated
        /// only if the first inner readiness probe returns `Pending`.
        #[must_use]
        pub fn reserve<'a>(&'a self, cx: &'a crate::cx::Cx) -> Reserve<'a, T> {
            Reserve {
                inner: self.inner.reserve(cx),
                retained_waker: None,
            }
        }

        /// Reserves and sends one value.
        pub async fn send(&self, cx: &crate::cx::Cx, value: T) -> Result<(), SendError<T>> {
            match self.reserve(cx).await {
                Ok(permit) => permit.try_send(value),
                Err(SendError::Disconnected(())) => Err(SendError::Disconnected(value)),
                Err(SendError::Full(())) => Err(SendError::Full(value)),
                Err(SendError::Cancelled(())) => Err(SendError::Cancelled(value)),
            }
        }

        pub fn try_reserve(&self) -> Result<SendPermit<'_, T>, SendError<()>> {
            self.inner.try_reserve().map(|inner| SendPermit { inner })
        }

        pub fn try_send(&self, value: T) -> Result<(), SendError<T>> {
            self.inner.try_send(value)
        }

        #[must_use]
        pub fn is_closed(&self) -> bool {
            self.inner.is_closed()
        }

        pub fn wake_receiver(&self) {
            self.inner.wake_receiver();
        }

        #[must_use]
        pub fn capacity(&self) -> usize {
            self.inner.capacity()
        }

        #[must_use]
        pub fn telemetry_snapshot(&self, channel_id: u64) -> MpscTelemetrySnapshot {
            self.inner.telemetry_snapshot(channel_id)
        }

        pub fn send_evict_oldest(&self, value: T) -> Result<Option<T>, SendError<T>> {
            self.inner.send_evict_oldest(value)
        }

        pub fn send_evict_oldest_where<F>(
            &self,
            value: T,
            predicate: F,
        ) -> Result<Option<T>, SendError<T>>
        where
            F: FnMut(&T) -> bool,
        {
            self.inner.send_evict_oldest_where(value, predicate)
        }

        #[must_use]
        pub fn downgrade(&self) -> WeakSender<T> {
            WeakSender {
                inner: self.inner.downgrade(),
            }
        }
    }

    /// Weak reference to a bounded MPSC sender.
    pub struct WeakSender<T> {
        inner: inner::WeakSender<T>,
    }

    impl<T> std::fmt::Debug for WeakSender<T> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("mpsc::WeakSender")
                .finish_non_exhaustive()
        }
    }

    impl<T> Clone for WeakSender<T> {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
            }
        }
    }

    impl<T> WeakSender<T> {
        #[must_use]
        pub fn upgrade(&self) -> Option<Sender<T>> {
            self.inner.upgrade().map(|inner| Sender { inner })
        }
    }

    /// Future returned by [`Sender::reserve`].
    pub struct Reserve<'a, T> {
        inner: inner::Reserve<'a, T>,
        retained_waker: Option<super::ContainedWakerBoundary>,
    }

    #[cfg(test)]
    impl<T> Reserve<'_, T> {
        pub(super) fn retained_waker_allocated_for_test(&self) -> bool {
            self.retained_waker.is_some()
        }
    }

    impl<'a, T> Future for Reserve<'a, T> {
        type Output = Result<SendPermit<'a, T>, SendError<()>>;

        #[inline]
        fn poll(self: Pin<&mut Self>, caller_cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            let Self {
                inner,
                retained_waker,
            } = this;
            let result = super::poll_with_durable_probe_contained_channel_waker(
                retained_waker,
                caller_cx,
                |proxy_cx| Pin::new(&mut *inner).poll(proxy_cx),
                new_waker_boundary,
                || Err(SendError::Cancelled(())),
            );
            result.map(|result| result.map(|inner| SendPermit { inner }))
        }
    }

    impl<T> Drop for Reserve<'_, T> {
        fn drop(&mut self) {
            if let Some(boundary) = &self.retained_waker {
                boundary.clear();
            }
        }
    }

    /// A reserved MPSC send slot.
    #[must_use = "SendPermit must be consumed via send() or abort()"]
    pub struct SendPermit<'a, T> {
        inner: inner::SendPermit<'a, T>,
    }

    impl<T> std::fmt::Debug for SendPermit<'_, T> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("mpsc::SendPermit")
                .finish_non_exhaustive()
        }
    }

    impl<T> SendPermit<'_, T> {
        pub fn send(self, value: T) -> asupersync::Outcome<(), SendError<T>> {
            self.inner.send(value)
        }

        pub fn try_send(self, value: T) -> Result<(), SendError<T>> {
            self.inner.try_send(value)
        }

        pub fn abort(self) {
            self.inner.abort();
        }

        #[must_use]
        pub fn telemetry_snapshot(&self, channel_id: u64) -> MpscTelemetrySnapshot {
            self.inner.telemetry_snapshot(channel_id)
        }
    }

    /// Project-owned receiving side of a bounded MPSC channel.
    ///
    /// Receive polls install the stable proxy before their first inner poll.
    /// They intentionally do not use the no-op fast probe because
    /// [`Sender::wake_receiver`] is an edge-triggered wake with no durable
    /// message or close transition for a follow-up probe to observe.
    pub struct Receiver<T> {
        inner: inner::Receiver<T>,
        retained_waker: Option<super::ContainedWakerBoundary>,
    }

    impl<T> std::fmt::Debug for Receiver<T> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("mpsc::Receiver")
                .finish_non_exhaustive()
        }
    }

    impl<T> Receiver<T> {
        #[cfg(test)]
        pub(super) fn retained_waker_allocated_for_test(&self) -> bool {
            self.retained_waker.is_some()
        }

        pub fn close(&mut self) {
            if let Some(boundary) = &self.retained_waker {
                boundary.clear();
            }
            self.inner.close();
        }

        #[must_use]
        pub fn recv<'a, Caps>(&'a mut self, cx: &'a crate::cx::Cx<Caps>) -> Recv<'a, T, Caps> {
            let Self {
                inner,
                retained_waker,
            } = self;
            Recv {
                inner: inner.recv(cx),
                retained_waker,
            }
        }

        #[must_use]
        pub fn recv_many<'a, Caps>(
            &'a mut self,
            cx: &'a crate::cx::Cx<Caps>,
            buffer: &'a mut Vec<T>,
            limit: usize,
        ) -> RecvMany<'a, T, Caps> {
            let Self {
                inner,
                retained_waker,
            } = self;
            RecvMany {
                inner: inner.recv_many(cx, buffer, limit),
                retained_waker,
            }
        }

        pub fn poll_recv<Caps>(
            &mut self,
            cx: &crate::cx::Cx<Caps>,
            caller_cx: &mut Context<'_>,
        ) -> Poll<Result<T, RecvError>> {
            let Self {
                inner,
                retained_waker,
            } = self;
            super::poll_with_contained_channel_waker(
                retained_waker,
                caller_cx,
                |proxy_cx| inner.poll_recv(cx, proxy_cx),
                new_waker_boundary,
                || Err(RecvError::Cancelled),
            )
        }

        pub fn poll_recv_many<Caps>(
            &mut self,
            cx: &crate::cx::Cx<Caps>,
            buffer: &mut Vec<T>,
            limit: usize,
            caller_cx: &mut Context<'_>,
        ) -> Poll<Result<usize, RecvError>> {
            let Self {
                inner,
                retained_waker,
            } = self;
            super::poll_with_contained_channel_waker(
                retained_waker,
                caller_cx,
                |proxy_cx| inner.poll_recv_many(cx, buffer, limit, proxy_cx),
                new_waker_boundary,
                || Err(RecvError::Cancelled),
            )
        }

        pub fn try_recv(&mut self) -> Result<T, RecvError> {
            let result = self.inner.try_recv();
            // `Empty` leaves the inner proxy registered for an earlier direct
            // receive poll, so its matching caller registration must survive.
            // Every other result consumes a value or terminal state and the
            // inner receiver removes that proxy; retire the caller in lockstep.
            if !matches!(&result, Err(RecvError::Empty)) {
                if let Some(boundary) = &self.retained_waker {
                    boundary.clear();
                }
            }
            result
        }

        #[must_use]
        pub fn is_closed(&self) -> bool {
            self.inner.is_closed()
        }

        #[must_use]
        pub fn has_messages(&self) -> bool {
            self.inner.has_messages()
        }

        #[must_use]
        pub fn len(&self) -> usize {
            self.inner.len()
        }

        #[must_use]
        pub fn is_empty(&self) -> bool {
            self.inner.is_empty()
        }

        #[must_use]
        pub fn capacity(&self) -> usize {
            self.inner.capacity()
        }

        #[must_use]
        pub fn telemetry_snapshot(&self, channel_id: u64) -> MpscTelemetrySnapshot {
            self.inner.telemetry_snapshot(channel_id)
        }
    }

    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            if let Some(boundary) = &self.retained_waker {
                boundary.clear();
            }
        }
    }

    /// Future returned by [`Receiver::recv`].
    ///
    /// The mutable forwarding-slot borrow keeps receiver-side operations from
    /// racing this wait. `Drop` clears the caller before the inner receive
    /// future releases its trusted proxy.
    pub struct Recv<'a, T, Caps = asupersync::cx::cap::All> {
        inner: inner::Recv<'a, T, Caps>,
        retained_waker: &'a mut Option<super::ContainedWakerBoundary>,
    }

    impl<T, Caps> Future for Recv<'_, T, Caps> {
        type Output = Result<T, RecvError>;

        #[inline]
        fn poll(self: Pin<&mut Self>, caller_cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            let Self {
                inner,
                retained_waker,
            } = this;
            super::poll_with_contained_channel_waker(
                retained_waker,
                caller_cx,
                |proxy_cx| Pin::new(&mut *inner).poll(proxy_cx),
                new_waker_boundary,
                || Err(RecvError::Cancelled),
            )
        }
    }

    impl<T, Caps> Drop for Recv<'_, T, Caps> {
        fn drop(&mut self) {
            if let Some(boundary) = self.retained_waker.as_ref() {
                boundary.clear();
            }
        }
    }

    /// Future returned by [`Receiver::recv_many`].
    ///
    /// The mutable forwarding-slot borrow keeps receiver-side operations from
    /// racing this wait. `Drop` clears the caller before the inner batch
    /// receive future releases its trusted proxy.
    pub struct RecvMany<'a, T, Caps = asupersync::cx::cap::All> {
        inner: inner::RecvMany<'a, T, Caps>,
        retained_waker: &'a mut Option<super::ContainedWakerBoundary>,
    }

    impl<T, Caps> Future for RecvMany<'_, T, Caps> {
        type Output = Result<usize, RecvError>;

        #[inline]
        fn poll(self: Pin<&mut Self>, caller_cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            let Self {
                inner,
                retained_waker,
            } = this;
            super::poll_with_contained_channel_waker(
                retained_waker,
                caller_cx,
                |proxy_cx| Pin::new(&mut *inner).poll(proxy_cx),
                new_waker_boundary,
                || Err(RecvError::Cancelled),
            )
        }
    }

    impl<T, Caps> Drop for RecvMany<'_, T, Caps> {
        fn drop(&mut self) {
            if let Some(boundary) = self.retained_waker.as_ref() {
                boundary.clear();
            }
        }
    }

    /// Compatibility alias for `try_send` errors, matching the tokio
    /// `TrySendError` API surface (`Full` / `Closed`).
    ///
    /// In asupersync the `try_send` method returns `SendError` which uses
    /// `Disconnected` instead of `Closed`. This wrapper bridges the naming
    /// gap so that call-sites can use `TrySendError::Full` / `Closed` uniformly.
    #[derive(Debug)]
    pub enum TrySendError<T> {
        /// The channel is full.
        Full(T),
        /// The receiver has been dropped.
        Closed(T),
    }

    impl<T> From<SendError<T>> for TrySendError<T> {
        fn from(err: SendError<T>) -> Self {
            match err {
                SendError::Full(v) => Self::Full(v),
                SendError::Disconnected(v) | SendError::Cancelled(v) => Self::Closed(v),
            }
        }
    }
}

/// Project-owned watch wrappers for the active runtime.
///
/// Under asupersync the receiver's `changed(cx)` method observes
/// **pre-cancel** on cx via the per-poll `cx.checkpoint()`
/// short-circuit (pinned by `watch_changed_with_cx_observes_pre_cancel`,
/// ft-xbnl0.2.4 tick 423).
///
/// Does NOT observe **mid-flight cancel**: asupersync's watch receiver
/// does not register a cx-cancel-waker (pinned by
/// `watch_changed_with_cx_mid_flight_cancel_via_select_race_pattern`,
/// tick 438 — extends the tick 432/433/434 finding from
/// mpsc/oneshot/broadcast to watch, confirming all four asupersync
/// channel types share the same design). Callers needing mid-flight
/// cancel observability must wrap in `futures::future::select`
/// against a poll-sleep watcher (same pattern as
/// `DistributedHttpClient::race_with_cx_cancel`, tick 387). See
/// `docs/ft-xbnl0-2-4-completion-evidence.md` §2.6.1.
pub mod watch {
    use asupersync::channel::watch as inner;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub use inner::{ModifyError, RecvError, Ref, SendError, WatchTelemetrySnapshot};

    static RETAINED_WAKER_LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    static RETAINED_WAKER_CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    /// Number of recovered forwarding-slot poison events for watch waiters.
    #[must_use]
    pub fn retained_waker_lock_poisoned_count() -> u64 {
        RETAINED_WAKER_LOCK_POISONED_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of caller waker clone, wake, or drop panics contained at the
    /// canonical watch boundary.
    #[must_use]
    pub fn retained_waker_callback_panic_count() -> u64 {
        RETAINED_WAKER_CALLBACK_PANIC_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn new_waker_boundary() -> super::ContainedWakerBoundary {
        super::ContainedWakerBoundary::new(
            &RETAINED_WAKER_LOCK_POISONED_COUNT,
            &RETAINED_WAKER_CALLBACK_PANIC_COUNT,
        )
    }

    /// Creates a watch channel.
    #[must_use]
    pub fn channel<T>(initial: T) -> (Sender<T>, Receiver<T>) {
        let (sender, receiver) = inner::channel(initial);
        (
            Sender { inner: sender },
            Receiver {
                inner: receiver,
                retained_waker: None,
            },
        )
    }

    /// Project-owned watch sender.
    pub struct Sender<T> {
        inner: inner::Sender<T>,
    }

    impl<T> std::fmt::Debug for Sender<T> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("watch::Sender")
                .finish_non_exhaustive()
        }
    }

    impl<T> Sender<T> {
        pub fn send(&self, value: T) -> Result<(), SendError<T>> {
            self.inner.send(value)
        }

        pub fn send_modify<F>(&self, mutation: F) -> Result<(), ModifyError>
        where
            T: Clone,
            F: FnOnce(&mut T),
        {
            self.inner.send_modify(mutation)
        }

        #[must_use]
        pub fn borrow(&self) -> Ref<'_, T> {
            self.inner.borrow()
        }

        #[must_use]
        pub fn subscribe(&self) -> Receiver<T> {
            Receiver {
                inner: self.inner.subscribe(),
                retained_waker: None,
            }
        }

        #[must_use]
        pub fn receiver_count(&self) -> usize {
            self.inner.receiver_count()
        }

        #[must_use]
        pub fn is_closed(&self) -> bool {
            self.inner.is_closed()
        }

        #[must_use]
        pub fn telemetry_snapshot(&self, channel_id: u64) -> WatchTelemetrySnapshot {
            self.inner.telemetry_snapshot(channel_id)
        }
    }

    /// Project-owned watch receiver. Its stable proxy is allocated only after
    /// the first inner `changed` probe returns `Pending`, then reused by that
    /// receiver.
    pub struct Receiver<T> {
        inner: inner::Receiver<T>,
        retained_waker: Option<super::ContainedWakerBoundary>,
    }

    impl<T> std::fmt::Debug for Receiver<T> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("watch::Receiver")
                .finish_non_exhaustive()
        }
    }

    impl<T> Clone for Receiver<T> {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                retained_waker: None,
            }
        }
    }

    impl<T> Receiver<T> {
        #[cfg(test)]
        pub(super) fn retained_waker_allocated_for_test(&self) -> bool {
            self.retained_waker.is_some()
        }

        #[must_use]
        pub fn changed<'a, 'b, Caps>(
            &'a mut self,
            cx: &'b crate::cx::Cx<Caps>,
        ) -> ChangedFuture<'a, 'b, T, Caps> {
            let Self {
                inner,
                retained_waker,
            } = self;
            ChangedFuture {
                inner: inner.changed(cx),
                retained_waker,
            }
        }

        #[must_use]
        pub fn borrow(&self) -> Ref<'_, T> {
            self.inner.borrow()
        }

        #[must_use]
        pub fn borrow_and_update(&mut self) -> Ref<'_, T> {
            self.inner.borrow_and_update()
        }

        #[must_use]
        pub fn borrow_and_clone(&self) -> T
        where
            T: Clone,
        {
            self.inner.borrow_and_clone()
        }

        #[must_use]
        pub fn borrow_and_update_clone(&mut self) -> T
        where
            T: Clone,
        {
            self.inner.borrow_and_update_clone()
        }

        pub fn mark_seen(&mut self) {
            self.inner.mark_seen();
        }

        #[must_use]
        pub fn has_changed(&self) -> bool {
            self.inner.has_changed()
        }

        #[must_use]
        pub fn is_closed(&self) -> bool {
            self.inner.is_closed()
        }

        #[must_use]
        pub fn seen_version(&self) -> u64 {
            self.inner.seen_version()
        }

        #[must_use]
        pub fn telemetry_snapshot(&self, channel_id: u64) -> WatchTelemetrySnapshot {
            self.inner.telemetry_snapshot(channel_id)
        }
    }

    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            if let Some(boundary) = &self.retained_waker {
                boundary.clear();
            }
        }
    }

    /// Future returned by [`Receiver::changed`].
    ///
    /// This future mutably borrows both the inner receiver and its forwarding
    /// slot. Receiver state-consuming methods therefore cannot run until this
    /// future is dropped, whose `Drop` clears the caller before the inner
    /// future releases its proxy registration.
    pub struct ChangedFuture<'a, 'b, T, Caps = asupersync::cx::cap::All> {
        inner: inner::ChangedFuture<'a, 'b, T, Caps>,
        retained_waker: &'a mut Option<super::ContainedWakerBoundary>,
    }

    impl<T, Caps> Future for ChangedFuture<'_, '_, T, Caps> {
        type Output = Result<(), RecvError>;

        #[inline]
        fn poll(self: Pin<&mut Self>, caller_cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            let Self {
                inner,
                retained_waker,
            } = this;
            super::poll_with_durable_probe_contained_channel_waker(
                retained_waker,
                caller_cx,
                |proxy_cx| Pin::new(&mut *inner).poll(proxy_cx),
                new_waker_boundary,
                || Err(RecvError::Cancelled),
            )
        }
    }

    impl<T, Caps> Drop for ChangedFuture<'_, '_, T, Caps> {
        fn drop(&mut self) {
            if let Some(boundary) = self.retained_waker.as_ref() {
                boundary.clear();
            }
        }
    }
}

/// Project-owned broadcast wrappers for the active runtime.
///
/// Provides wrapper types around `asupersync::channel::broadcast` that acquire
/// a `Cx` internally while retaining the established call-site signatures.
pub mod broadcast {
    use asupersync::channel::broadcast as inner;
    use std::future::Future;

    static RETAINED_WAKER_LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    static RETAINED_WAKER_CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    /// Number of recovered forwarding-slot poison events for broadcast waiters.
    #[must_use]
    pub fn retained_waker_lock_poisoned_count() -> u64 {
        RETAINED_WAKER_LOCK_POISONED_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of caller waker clone, wake, or drop panics contained at the
    /// canonical broadcast boundary.
    #[must_use]
    pub fn retained_waker_callback_panic_count() -> u64 {
        RETAINED_WAKER_CALLBACK_PANIC_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn new_waker_boundary() -> super::ContainedWakerBoundary {
        super::ContainedWakerBoundary::new(
            &RETAINED_WAKER_LOCK_POISONED_COUNT,
            &RETAINED_WAKER_CALLBACK_PANIC_COUNT,
        )
    }

    /// Error returned when sending fails (no active receivers).
    #[derive(Debug)]
    pub struct SendError<T>(pub T);

    impl<T> std::fmt::Display for SendError<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "sending on a closed broadcast channel")
        }
    }

    impl<T: std::fmt::Debug> std::error::Error for SendError<T> {}

    /// Error returned when receiving fails.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RecvError {
        /// The receiver fell behind and missed messages.
        Lagged(u64),
        /// All senders have been dropped.
        Closed,
        /// The capability context was cancelled.
        Cancelled,
    }

    impl std::fmt::Display for RecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Lagged(n) => write!(f, "receiver lagged by {n} messages"),
                Self::Closed => write!(f, "broadcast channel closed"),
                Self::Cancelled => write!(f, "broadcast receive cancelled"),
            }
        }
    }

    impl std::error::Error for RecvError {}

    /// Error returned by non-blocking try_recv.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TryRecvError {
        /// No message available right now.
        Empty,
        /// All senders have been dropped.
        Closed,
        /// The receiver fell behind and missed messages.
        Lagged(u64),
    }

    impl std::fmt::Display for TryRecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Empty => write!(f, "broadcast channel empty"),
                Self::Closed => write!(f, "broadcast channel closed"),
                Self::Lagged(n) => write!(f, "receiver lagged by {n} messages"),
            }
        }
    }

    impl std::error::Error for TryRecvError {}

    /// Wrapper around [`asupersync::channel::broadcast::Sender`] that acquires
    /// a `Cx` internally, preserving the tokio-compatible `.send(value)` API.
    pub struct Sender<T> {
        inner: inner::Sender<T>,
    }

    impl<T> std::fmt::Debug for Sender<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("broadcast::Sender").finish_non_exhaustive()
        }
    }

    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
            }
        }
    }

    impl<T: Clone> Sender<T> {
        /// Sends a message to all receivers.
        ///
        /// Acquires a `Cx` internally for the asupersync two-phase send.
        /// Returns `Ok(receiver_count)` or `Err(SendError(value))` if no
        /// receivers are alive.
        pub fn send(&self, value: T) -> Result<usize, SendError<T>> {
            let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            self.send_with_cx(&cx, value)
        }

        /// Sends a message to all receivers under an explicit `&Cx`
        /// (ft-xbnl0.2.x Cx-first primitive). Preferred over
        /// [`send`](Self::send) when the caller already threads `&Cx`
        /// through its public API — the Cx flows into the asupersync
        /// two-phase reserve/commit send instead of being pulled from
        /// thread-local state.
        pub fn send_with_cx(&self, cx: &crate::cx::Cx, value: T) -> Result<usize, SendError<T>> {
            self.inner.send(cx, value).map_err(|err| match err {
                inner::SendError::Closed(v) | inner::SendError::Cancelled(v) => SendError(v),
            })
        }

        /// Creates a new receiver subscribed to this channel.
        pub fn subscribe(&self) -> Receiver<T> {
            Receiver {
                inner: self.inner.subscribe(),
                retained_waker: None,
            }
        }

        /// Returns the number of active receivers.
        pub fn receiver_count(&self) -> usize {
            self.inner.receiver_count()
        }

        /// Returns the number of messages currently buffered.
        pub fn len(&self) -> usize {
            self.inner.len()
        }

        /// Returns `true` if no messages are buffered.
        pub fn is_empty(&self) -> bool {
            self.inner.is_empty()
        }
    }

    /// Wrapper around [`asupersync::channel::broadcast::Receiver`] that
    /// acquires a `Cx` internally for async recv.
    pub struct Receiver<T> {
        inner: inner::Receiver<T>,
        retained_waker: Option<super::ContainedWakerBoundary>,
    }

    impl<T> std::fmt::Debug for Receiver<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("broadcast::Receiver")
                .finish_non_exhaustive()
        }
    }

    impl<T> Clone for Receiver<T> {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                retained_waker: None,
            }
        }
    }

    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            if let Some(boundary) = &self.retained_waker {
                boundary.clear();
            }
        }
    }

    impl<T: Clone> Receiver<T> {
        #[cfg(test)]
        pub(super) fn retained_waker_allocated_for_test(&self) -> bool {
            self.retained_waker.is_some()
        }

        /// Receives the next message.
        ///
        /// Acquires a `Cx` internally for the asupersync async recv.
        pub async fn recv(&mut self) -> Result<T, RecvError> {
            let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            self.recv_with_cx(&cx).await
        }

        /// Receives the next message under an explicit `&Cx` (ft-xbnl0.2.2
        /// Cx-first API). Cancellation, budget, and virtual time all flow
        /// through the provided capability context instead of being pulled
        /// from thread-local state.
        #[must_use]
        pub fn recv_with_cx<'a>(&'a mut self, cx: &'a crate::cx::Cx) -> Recv<'a, T> {
            let Self {
                inner,
                retained_waker,
            } = self;
            Recv {
                inner: inner.recv(cx),
                retained_waker,
            }
        }

        /// Attempts to receive the next message without blocking.
        pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
            self.inner.try_recv().map_err(|e| match e {
                inner::TryRecvError::Empty => TryRecvError::Empty,
                inner::TryRecvError::Closed => TryRecvError::Closed,
                inner::TryRecvError::Lagged(n) => TryRecvError::Lagged(n),
            })
        }
    }

    /// Future returned by [`Receiver::recv_with_cx`].
    ///
    /// This future mutably borrows both the inner receiver and its forwarding
    /// slot. `try_recv` therefore cannot run until this future is dropped,
    /// whose `Drop` clears the caller before the inner future releases its
    /// proxy registration.
    pub struct Recv<'a, T> {
        inner: inner::Recv<'a, T>,
        retained_waker: &'a mut Option<super::ContainedWakerBoundary>,
    }

    impl<T: Clone> Future for Recv<'_, T> {
        type Output = Result<T, RecvError>;

        #[inline]
        fn poll(
            self: std::pin::Pin<&mut Self>,
            caller_cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            let this = self.get_mut();
            let Self {
                inner,
                retained_waker,
            } = this;
            super::poll_with_durable_probe_contained_channel_waker(
                retained_waker,
                caller_cx,
                |proxy_cx| std::pin::Pin::new(&mut *inner).poll(proxy_cx),
                new_waker_boundary,
                || Err(inner::RecvError::Cancelled),
            )
            .map(|result| {
                result.map_err(|error| match error {
                    inner::RecvError::Lagged(missed) => RecvError::Lagged(missed),
                    inner::RecvError::Closed => RecvError::Closed,
                    inner::RecvError::Cancelled => RecvError::Cancelled,
                    inner::RecvError::PolledAfterCompletion => RecvError::Closed,
                })
            })
        }
    }

    impl<T> Drop for Recv<'_, T> {
        fn drop(&mut self) {
            if let Some(boundary) = self.retained_waker.as_ref() {
                boundary.clear();
            }
        }
    }

    /// Creates a new broadcast channel with the given capacity.
    pub fn channel<T: Clone>(capacity: usize) -> (Sender<T>, Receiver<T>) {
        let (tx, rx) = inner::channel(capacity);
        (
            Sender { inner: tx },
            Receiver {
                inner: rx,
                retained_waker: None,
            },
        )
    }
}

/// Oneshot channel aliases for the active runtime.
///
/// Provides wrapper types around `asupersync::channel::oneshot` that acquire a
/// `Cx` internally while retaining the established `Sender::send(value)`
/// signature.
///
/// `Receiver` does **not** impl `Future` under asupersync — callers that
/// previously used `rx.await` must go through [`oneshot_recv`] instead.
pub mod oneshot {
    use asupersync::channel::oneshot as inner;

    pub(super) static RECEIVER_WAKER_LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    pub(super) static RECEIVER_WAKER_CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    /// Returns the number of recovered oneshot receiver forwarding-slot
    /// poison events observed by this process.
    ///
    /// Caller `RawWaker` callbacks run outside the slot lock, so a non-zero
    /// value indicates historical poison or an internal invariant failure that
    /// should be investigated.
    #[must_use]
    pub fn receiver_waker_lock_poisoned_count() -> u64 {
        RECEIVER_WAKER_LOCK_POISONED_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns the number of caller waker callback panics contained while
    /// receiving from oneshot channels.
    #[must_use]
    pub fn receiver_waker_callback_panic_count() -> u64 {
        RECEIVER_WAKER_CALLBACK_PANIC_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn reset_receiver_waker_lock_poisoned_count_for_test() {
        RECEIVER_WAKER_LOCK_POISONED_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
        RECEIVER_WAKER_CALLBACK_PANIC_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Compatibility error type matching tokio's `oneshot::error::RecvError`.
    #[derive(Debug)]
    pub struct RecvError;

    impl std::fmt::Display for RecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "channel closed")
        }
    }

    impl std::error::Error for RecvError {}

    /// Wrapper around [`asupersync::channel::oneshot::Sender`] that acquires
    /// a `Cx` internally, preserving the tokio-compatible `.send(value)` API.
    pub struct Sender<T> {
        inner: inner::Sender<T>,
    }

    impl<T> std::fmt::Debug for Sender<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("oneshot::Sender").finish_non_exhaustive()
        }
    }

    /// Wrapper around [`asupersync::channel::oneshot::Receiver`].
    ///
    /// Does **not** implement `Future` — use [`super::oneshot_recv`] to
    /// receive from the channel asynchronously.
    pub struct Receiver<T> {
        pub(super) inner: inner::Receiver<T>,
    }

    impl<T> std::fmt::Debug for Receiver<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("oneshot::Receiver").finish_non_exhaustive()
        }
    }

    /// Creates a new oneshot channel.
    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let (tx, rx) = inner::channel();
        (Sender { inner: tx }, Receiver { inner: rx })
    }

    impl<T> Sender<T> {
        /// Sends a value on the channel.
        ///
        /// Acquires a [`Cx`](crate::cx::Cx) internally via
        /// `Cx::current()` (falling back to `for_request()`) for the
        /// asupersync two-phase reserve/commit send.
        ///
        /// Returns `Err(value)` if the receiver was dropped.
        pub fn send(self, value: T) -> Result<(), T> {
            let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            self.send_with_cx(&cx, value)
        }

        /// Sends a value on the channel under an explicit `&Cx`
        /// (ft-xbnl0.2.x Cx-first primitive). Preferred over
        /// [`send`](Self::send) when the caller already threads `&Cx`.
        ///
        /// Consumes `self` because oneshot senders fire at most once.
        /// Returns `Err(value)` if the receiver was dropped.
        pub fn send_with_cx(self, cx: &crate::cx::Cx, value: T) -> Result<(), T> {
            self.inner.send(cx, value).map_err(|err| match err {
                inner::SendError::Disconnected(v) | inner::SendError::Cancelled(v) => v,
            })
        }
    }
}

/// Async notification primitive backed by asupersync.
pub mod notify {
    pub use asupersync::sync::Notify;
}

/// Task primitives for the asupersync runtime.
///
/// Provides API-compatible wrappers around asupersync's spawn/join
/// infrastructure, using the thread-local `ASUPERSYNC_HANDLE` installed
/// by `Runtime::block_on` to support ambient spawning.
pub mod task {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;

    pub use crate::cx::SpawnError;

    // ─── br-ft-iaxog: JoinHandle downstream-waker poison recovery ──
    //
    // Pre-fix the production lock-sites on JoinHandle's caller-waker slot
    // (poll-store, poll-abort-clear, poll-success-clear, abort-wake)
    // used `.expect("abort waker mutex poisoned")`. The hot-path site
    // is `poll()`, called by the executor on EVERY task wakeup — a
    // panic in any thread holding the Mutex turned every subsequent
    // poll into a re-panic, killing the executor and bringing down the
    // entire runtime.
    //
    // Post-fix: `ContainedForwardingWaker` recovers via
    // `PoisonError::into_inner()` and bumps this counter. Recovery
    // cost: an inconsistent waker Option (a stale Waker stored, or a
    // notification dropped). That can cause a bounded spurious or delayed
    // poll, and the counter makes it visible; the pre-fix cascade cost was
    // runtime death.
    //
    // Same observability defect family as ft-luav8 / ft-skec1 /
    // ft-tpdl5 / ft-wzk10 / ft-4socw / ft-4pxzi / ft-as3w7 /
    // ft-h2vyr — make silent state loss visible (the counter), and
    // prevent runtime cascade when possible (the recovery).
    static JOIN_HANDLE_LOCK_POISONED_COUNT: AtomicU64 = AtomicU64::new(0);
    static JOIN_HANDLE_WAKER_CALLBACK_PANIC_COUNT: AtomicU64 = AtomicU64::new(0);
    const JOIN_SET_QUARANTINE_POLL_INTERVAL: Duration = Duration::from_millis(1);

    /// Read the current count of recovered JoinHandle downstream-waker
    /// Mutex-poison events. Non-zero values mean a prior thread
    /// panicked while holding a JoinHandle downstream-waker slot; the runtime
    /// continued after recovering via `PoisonError::into_inner()`
    /// instead of cascading.
    #[must_use]
    pub fn join_handle_lock_poisoned_count() -> u64 {
        JOIN_HANDLE_LOCK_POISONED_COUNT.load(Ordering::Relaxed)
    }

    /// Read the number of caller waker callback panics contained by task join
    /// forwarding boundaries.
    #[must_use]
    pub fn join_handle_waker_callback_panic_count() -> u64 {
        JOIN_HANDLE_WAKER_CALLBACK_PANIC_COUNT.load(Ordering::Relaxed)
    }

    /// Test-only: reset the counter to zero.
    #[cfg(test)]
    pub(crate) fn reset_join_handle_lock_poisoned_count_for_test() {
        JOIN_HANDLE_LOCK_POISONED_COUNT.store(0, Ordering::Relaxed);
        JOIN_HANDLE_WAKER_CALLBACK_PANIC_COUNT.store(0, Ordering::Relaxed);
    }

    /// Finite failure class for a spawned-task join boundary.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum JoinErrorKind {
        /// The task future acknowledged an explicit abort request.
        Aborted,
        /// The caller's capability context was cancelled.
        ContextCancelled,
        /// The caller's capability deadline elapsed.
        DeadlineExceeded,
        /// The caller exhausted its cooperative poll quota.
        PollQuotaExhausted,
        /// The caller exhausted its cost budget.
        CostBudgetExhausted,
        /// A checkpoint failed without an attributable root cause.
        ContextFailure,
        /// The task panicked or otherwise failed at its join boundary.
        TaskFailed,
        /// The downstream completion waker could not be registered.
        WakerRegistrationFailed,
    }

    /// Error type returned when a spawned task fails.
    ///
    /// Panic payloads, cancellation messages, and caller-provided reason text
    /// never cross this finite, content-free boundary.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct JoinError {
        kind: JoinErrorKind,
    }

    impl std::fmt::Display for JoinError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let message = match self.kind {
                JoinErrorKind::Aborted => "task aborted",
                JoinErrorKind::ContextCancelled => "join cancelled by capability context",
                JoinErrorKind::DeadlineExceeded => "capability deadline exceeded at join",
                JoinErrorKind::PollQuotaExhausted => "capability poll quota exhausted at join",
                JoinErrorKind::CostBudgetExhausted => "capability cost budget exhausted at join",
                JoinErrorKind::ContextFailure => "capability checkpoint failed at join",
                JoinErrorKind::TaskFailed => "task failed at join boundary",
                JoinErrorKind::WakerRegistrationFailed => {
                    "join completion waker registration failed"
                }
            };
            write!(f, "JoinError: {message}")
        }
    }

    impl JoinError {
        /// Construct a finite task-aborted error.
        #[must_use]
        pub const fn aborted() -> Self {
            Self {
                kind: JoinErrorKind::Aborted,
            }
        }

        /// Construct a finite task-failed error.
        #[must_use]
        pub const fn task_failed() -> Self {
            Self {
                kind: JoinErrorKind::TaskFailed,
            }
        }

        const fn waker_registration_failed() -> Self {
            Self {
                kind: JoinErrorKind::WakerRegistrationFailed,
            }
        }

        fn from_context_failure(cx: &crate::cx::Cx) -> Self {
            use crate::outcome::CancelKind;

            let kind = match cx.root_cancel_cause().map(|reason| reason.kind) {
                Some(CancelKind::Deadline | CancelKind::Timeout) => JoinErrorKind::DeadlineExceeded,
                Some(CancelKind::PollQuota) => JoinErrorKind::PollQuotaExhausted,
                Some(CancelKind::CostBudget) => JoinErrorKind::CostBudgetExhausted,
                Some(
                    CancelKind::User
                    | CancelKind::FailFast
                    | CancelKind::RaceLost
                    | CancelKind::ParentCancelled
                    | CancelKind::ResourceUnavailable
                    | CancelKind::Shutdown
                    | CancelKind::LinkedExit,
                ) => JoinErrorKind::ContextCancelled,
                None => JoinErrorKind::ContextFailure,
            };
            Self { kind }
        }

        /// Return the finite structural failure class.
        #[must_use]
        pub const fn kind(&self) -> JoinErrorKind {
            self.kind
        }

        /// Return true for task abort or caller-context cancellation.
        #[must_use]
        pub const fn is_cancelled(&self) -> bool {
            matches!(
                self.kind,
                JoinErrorKind::Aborted | JoinErrorKind::ContextCancelled
            )
        }
    }

    impl std::error::Error for JoinError {}

    type AbortableTaskJoin<T> =
        asupersync::runtime::JoinHandle<std::result::Result<T, futures::future::Aborted>>;

    /// Handle to a spawned task. Awaiting it yields the task's output
    /// wrapped in `Result<T, JoinError>` for API compatibility with tokio.
    ///
    /// Uses `Pin<Box<_>>` internally to avoid unsafe pin projection while
    /// maintaining `#![forbid(unsafe_code)]` compliance.
    ///
    /// `abort()` signals the abortable task future. Ordinary abort completion
    /// is reported only after the runtime polls and drops that future, so
    /// draining an `Aborted` handle acknowledges that the async task future no
    /// longer exists. `WakerRegistrationFailed` is the sole fail-closed
    /// exception: the caller waker itself could not be quarantined, so the
    /// wrapper requests abort and returns an observation-boundary error without
    /// claiming that the task has acknowledged it.
    /// If that future already delegated work to a non-interruptible OS thread
    /// (notably [`spawn_blocking`]), acknowledgement does not prove the
    /// delegated closure stopped.
    pub struct JoinHandle<T> {
        inner: Pin<Box<AbortableTaskJoin<T>>>,
        abort_handle: futures::future::AbortHandle,
        forwarding: std::sync::Arc<super::ContainedForwardingWaker>,
        completion_waker: std::task::Waker,
        #[cfg(test)]
        force_registration_failure: std::sync::atomic::AtomicBool,
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            // Publish the caller waker before polling the inner join. The
            // asupersync handle sees only `completion_waker`, whose identity
            // remains stable across every poll and whose callback is contained.
            #[cfg(test)]
            if self.force_registration_failure.load(Ordering::Acquire) {
                self.abort_handle.abort();
                self.forwarding.clear();
                return Poll::Ready(Err(JoinError::waker_registration_failed()));
            }
            let registration = self.forwarding.register(cx.waker());
            if registration.is_err() {
                // If completion can no longer be forwarded, do not silently
                // detach work whose outcome has become unobservable.
                self.abort_handle.abort();
                self.forwarding.clear();
                return Poll::Ready(Err(JoinError::waker_registration_failed()));
            }

            self.as_mut().get_mut().poll_inner_with_trusted_waker()
        }
    }

    impl<T> JoinHandle<T> {
        /// Poll only the scheduler-owned inner join with the stable contained
        /// completion waker. This bypasses caller-waker registration and is
        /// reserved for retaining terminal authority after registration itself
        /// failed. A `Pending` result is not settlement and the handle must be
        /// retained for a later trusted poll.
        fn poll_inner_with_trusted_waker(&mut self) -> Poll<Result<T, JoinError>> {
            let mut completion_cx = Context::from_waker(&self.completion_waker);
            let inner_poll = frankenterm_sigpipe::catch_recoverable(
                frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
                std::panic::AssertUnwindSafe(|| self.inner.as_mut().poll(&mut completion_cx)),
            );
            match inner_poll {
                Ok(Poll::Ready(Ok(value))) => {
                    self.forwarding.clear();
                    Poll::Ready(Ok(value))
                }
                Ok(Poll::Ready(Err(_aborted))) => {
                    self.forwarding.clear();
                    Poll::Ready(Err(JoinError::aborted()))
                }
                Ok(Poll::Pending) => Poll::Pending,
                Err(_panic) => {
                    // Asupersync resumes the task panic while its join handle
                    // is polled. Convert it at this canonical boundary so the
                    // advertised `Result<T, JoinError>` contract is real and
                    // no caller has to catch an opaque task payload itself.
                    // The payload is disposed by `catch_recoverable`; retain
                    // only a finite, content-free failure class.
                    self.forwarding.clear();
                    Poll::Ready(Err(JoinError::task_failed()))
                }
            }
        }

        /// Returns `true` once the task has actually reached a terminal state.
        pub fn is_finished(&self) -> bool {
            self.inner.is_finished()
        }

        /// Request cancellation of the task.
        ///
        /// Wakes both the abortable task and the current join waiter. Awaiting
        /// the handle reports cancellation only after the task future has been
        /// dropped by the runtime.
        pub fn abort(&self) {
            self.abort_handle.abort();
            self.forwarding.forward_one();
        }

        #[cfg(test)]
        pub(crate) fn force_registration_failure_for_test(&self) {
            self.force_registration_failure
                .store(true, Ordering::Release);
        }
    }

    impl<T> Drop for JoinHandle<T> {
        fn drop(&mut self) {
            // The detached asupersync task may still hold the stable proxy.
            // Retire caller state before dropping the inner handle so a later
            // detached completion observes an empty downstream slot.
            self.forwarding.clear();
        }
    }

    /// Finite ownership state for a [`JoinSet`] drain.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum JoinSetSettlement {
        /// Every task handle reached terminal acknowledgement and was removed.
        Settled,
        /// The set still owns task handles that have not terminally settled.
        Incomplete {
            /// Handles whose caller-waker observation path remains usable.
            active_tasks: usize,
            /// Aborted handles retained after caller-waker registration failed.
            unacknowledged_tasks: usize,
        },
    }

    /// Minimal JoinSet implementation backed by owned `JoinHandle` vectors.
    ///
    /// Provides the subset of tokio::task::JoinSet API used in frankenterm.
    pub struct JoinSet<T> {
        handles: Vec<JoinHandle<T>>,
        /// Handles for which caller-waker registration failed after abort was
        /// requested but before terminal acknowledgement was observable. They
        /// are polled only through the trusted inner completion waker.
        unacknowledged: Vec<JoinHandle<T>>,
    }

    impl<T: Send + 'static> Default for JoinSet<T> {
        fn default() -> Self {
            Self {
                handles: Vec::new(),
                unacknowledged: Vec::new(),
            }
        }
    }

    impl<T: Send + 'static> JoinSet<T> {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn spawn<F>(&mut self, future: F)
        where
            F: Future<Output = T> + Send + 'static,
        {
            self.handles.push(super::task::spawn(future));
        }

        pub fn spawn_with_cx<F, Fut>(&mut self, cx: &crate::cx::Cx, task: F)
        where
            F: FnOnce(crate::cx::Cx) -> Fut + Send + 'static,
            Fut: Future<Output = T> + Send + 'static,
        {
            self.handles.push(super::task::spawn_with_cx(cx, task));
        }

        /// Adopt an already-admitted task handle while preserving this set's
        /// terminal-drain ownership contract.
        pub(crate) fn insert_handle(&mut self, handle: JoinHandle<T>) {
            self.handles.push(handle);
        }

        pub fn len(&self) -> usize {
            self.handles.len().saturating_add(self.unacknowledged.len())
        }

        pub fn is_empty(&self) -> bool {
            self.handles.is_empty() && self.unacknowledged.is_empty()
        }

        /// Number of aborted handles still awaiting terminal acknowledgement
        /// after caller-waker registration failed.
        pub fn unacknowledged_len(&self) -> usize {
            self.unacknowledged.len()
        }

        /// Snapshot whether every owned task has terminally settled.
        pub fn settlement(&self) -> JoinSetSettlement {
            if self.is_empty() {
                JoinSetSettlement::Settled
            } else {
                JoinSetSettlement::Incomplete {
                    active_tasks: self.handles.len(),
                    unacknowledged_tasks: self.unacknowledged.len(),
                }
            }
        }

        fn poll_finished_unacknowledged(&mut self) -> Option<Result<T, JoinError>> {
            for index in 0..self.unacknowledged.len() {
                if !self.unacknowledged[index].is_finished() {
                    continue;
                }
                if let Poll::Ready(result) =
                    self.unacknowledged[index].poll_inner_with_trusted_waker()
                {
                    self.unacknowledged.swap_remove(index);
                    return Some(result);
                }
            }
            None
        }

        fn settle_or_quarantine_registration_failure(
            &mut self,
            index: usize,
            observation_error: JoinError,
        ) -> Result<T, JoinError> {
            match self.handles[index].poll_inner_with_trusted_waker() {
                Poll::Ready(result) => {
                    self.handles.swap_remove(index);
                    result
                }
                Poll::Pending => {
                    let handle = self.handles.swap_remove(index);
                    self.unacknowledged.push(handle);
                    Err(observation_error)
                }
            }
        }

        fn poll_active(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, JoinError>>> {
            for index in 0..self.handles.len() {
                let poll = Pin::new(&mut self.handles[index]).poll(cx);
                match poll {
                    Poll::Ready(Err(error))
                        if error.kind() == JoinErrorKind::WakerRegistrationFailed =>
                    {
                        let result = self.settle_or_quarantine_registration_failure(index, error);
                        return Poll::Ready(Some(result));
                    }
                    Poll::Ready(result) => {
                        self.handles.swap_remove(index);
                        return Poll::Ready(Some(result));
                    }
                    Poll::Pending => {}
                }
            }
            Poll::Pending
        }

        /// Await the next completed task.
        ///
        /// A [`JoinErrorKind::WakerRegistrationFailed`] observation requests
        /// abort and polls once through a trusted internal waker. If terminal
        /// acknowledgement is not ready, the handle is quarantined and the
        /// finite observation error is returned exactly once. A later call can
        /// drain it after [`JoinHandle::is_finished`] becomes true. `None` can
        /// therefore mean either an empty set or that only unacknowledged
        /// quarantined handles remain; callers making a settlement claim must
        /// inspect [`Self::settlement`].
        pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
            if self.is_empty() {
                return None;
            }

            std::future::poll_fn(|cx| {
                if let Some(result) = self.poll_finished_unacknowledged() {
                    return Poll::Ready(Some(result));
                }
                if self.handles.is_empty() {
                    return Poll::Ready(None);
                }
                self.poll_active(cx)
            })
            .await
        }

        /// ft-xbnl0.2.3 Cx-first sibling of [`JoinSet::join_next`].
        ///
        /// Pre-flight `cx.checkpoint()` gates entry, and each
        /// subsequent poll loop also invokes `cx.checkpoint()`
        /// before scanning the handles so a cancelled caller
        /// surfaces within the same tick as the cancel signal
        /// rather than waiting for the next task completion.
        /// Returns `Some(Err(JoinError))` on context failure. The finite
        /// `JoinErrorKind` preserves cancellation-vs-budget distinctions
        /// without copying caller-provided cancellation text.
        /// `WakerRegistrationFailed` follows the same one-shot quarantine and
        /// trusted-terminal-poll contract as [`Self::join_next`].
        ///
        /// Note the local shadowing of `cx`: the poll_fn closure
        /// receives a `std::task::Context` also conventionally
        /// named `cx`, so the outer capability context is bound
        /// as `caller_cx` to avoid name collision inside the
        /// poll body.
        ///
        /// # Cancellation semantics
        ///
        /// Observes **pre-cancel**: the pre-flight checkpoint fires
        /// before any handle polling (pinned by
        /// `join_set_join_next_with_cx_observes_pre_cancel`,
        /// ft-xbnl0.2.4 tick 426). Returns
        /// `Some(Err(JoinError))`; `JoinError::is_cancelled()` is true only
        /// for an actual cancellation class, not deadline/poll/cost budgets.
        ///
        /// Also observes **mid-flight cancel** on any external re-poll
        /// (task completion, external wake) via the per-poll-iteration
        /// `caller_cx.checkpoint()` inside the `poll_fn` closure — this
        /// is the key distinction from the asupersync-delegated recv
        /// primitives (mpsc/oneshot/broadcast/watch/Semaphore) which
        /// require the caller-side select-race workaround for
        /// mid-flight cancel. The tick-439b select-race test tolerates
        /// either branch firing; both observe cancel fast. See
        /// `docs/ft-xbnl0-2-4-completion-evidence.md` §2.6.1.
        pub async fn join_next_with_cx(
            &mut self,
            caller_cx: &crate::cx::Cx,
        ) -> Option<Result<T, JoinError>> {
            if self.is_empty() {
                return None;
            }
            if caller_cx.checkpoint().is_err() {
                return Some(Err(JoinError::from_context_failure(caller_cx)));
            }

            std::future::poll_fn(|cx| {
                if caller_cx.checkpoint().is_err() {
                    return std::task::Poll::Ready(Some(Err(JoinError::from_context_failure(
                        caller_cx,
                    ))));
                }
                if let Some(result) = self.poll_finished_unacknowledged() {
                    return Poll::Ready(Some(result));
                }
                if self.handles.is_empty() {
                    return Poll::Ready(None);
                }
                self.poll_active(cx)
            })
            .await
        }

        /// Drain one terminal task result without treating quarantined handles
        /// as an empty set.
        ///
        /// Unlike [`Self::join_next`] and [`Self::join_next_with_cx`], `Ok(None)`
        /// is returned only when this set owns no active or unacknowledged
        /// handles. If caller-waker registration failed and the affected task
        /// has not yet acknowledged the requested abort, this method retains
        /// the handle and periodically polls it through the trusted internal
        /// completion waker until it becomes terminal.
        ///
        /// The retry interval prevents a quarantined task from turning a
        /// shutdown lane into a hot spin. Callers must still put their desired
        /// overall shutdown deadline around this future (for example with
        /// [`super::timeout_with_cx`]); dropping the future leaves terminal
        /// authority in this `JoinSet`, visible through [`Self::settlement`].
        /// A top-level `Err` is a finite drain-context failure and does not
        /// consume any owned task handle. The nested result is the completed
        /// task's ordinary join result.
        pub async fn drain_next_with_cx(
            &mut self,
            caller_cx: &crate::cx::Cx,
        ) -> Result<Option<Result<T, JoinError>>, JoinError> {
            loop {
                if self.is_empty() {
                    return Ok(None);
                }
                if caller_cx.checkpoint().is_err() {
                    return Err(JoinError::from_context_failure(caller_cx));
                }
                if let Some(result) = self.try_join_next() {
                    return Ok(Some(result));
                }

                if !self.unacknowledged.is_empty() {
                    // A quarantined handle's stable completion waker no longer
                    // forwards to this caller. Always arm the retry timer when
                    // any such authority remains, even if another active task
                    // is also pending; otherwise that unrelated active task
                    // could suppress the only trusted re-poll until the outer
                    // shutdown deadline expires.
                    super::sleep_with_cx(caller_cx, JOIN_SET_QUARANTINE_POLL_INTERVAL)
                        .await
                        .map_err(|_| JoinError::from_context_failure(caller_cx))?;
                    continue;
                }

                if !self.handles.is_empty() {
                    let result = std::future::poll_fn(|task_cx| {
                        if caller_cx.checkpoint().is_err() {
                            return Poll::Ready(Err(JoinError::from_context_failure(caller_cx)));
                        }
                        match self.poll_active(task_cx) {
                            Poll::Ready(result) => Poll::Ready(Ok(result)),
                            Poll::Pending => Poll::Pending,
                        }
                    })
                    .await?;
                    if result.is_some() {
                        return Ok(result);
                    }
                }
            }
        }

        /// Drain one terminal task result while retaining unconditional
        /// settlement authority.
        ///
        /// This is the unbounded counterpart to [`Self::drain_next_with_cx`]
        /// for ownership boundaries whose public contract is to join every
        /// task rather than return on caller-context failure. Quarantined
        /// handles are polled through their stable completion waker after a
        /// small unbudgeted timer pause, so a persistent caller-waker
        /// registration failure cannot detach work or turn into a hot spin.
        pub(crate) async fn drain_next_trusted(&mut self) -> Option<Result<T, JoinError>> {
            loop {
                if self.is_empty() {
                    return None;
                }
                if let Some(result) = self.try_join_next() {
                    return Some(result);
                }

                if !self.unacknowledged.is_empty() {
                    super::sleep_unbudgeted(JOIN_SET_QUARANTINE_POLL_INTERVAL).await;
                    continue;
                }

                if !self.handles.is_empty() {
                    let result = std::future::poll_fn(|task_cx| self.poll_active(task_cx)).await;
                    if result.is_some() {
                        return result;
                    }
                }
            }
        }

        /// Non-blocking poll for the next completed task.
        ///
        /// Checks if any handle is finished and returns its result.
        /// Returns `None` if the set is empty or no task has completed.
        /// A [`JoinErrorKind::WakerRegistrationFailed`] observation uses the
        /// same trusted-poll/quarantine contract as [`Self::join_next`], so a
        /// repeated caller cannot spin on the same nonterminal error.
        pub fn try_join_next(&mut self) -> Option<Result<T, JoinError>> {
            if let Some(result) = self.poll_finished_unacknowledged() {
                return Some(result);
            }
            // Find the first finished handle
            let pos = self.handles.iter().position(|h| h.is_finished());
            if let Some(idx) = pos {
                // Task is finished, so we can poll it synchronously via a noop waker
                let waker = futures::task::noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                let result = std::pin::Pin::new(&mut self.handles[idx]).poll(&mut cx);
                match result {
                    std::task::Poll::Ready(Err(error))
                        if error.kind() == JoinErrorKind::WakerRegistrationFailed =>
                    {
                        Some(self.settle_or_quarantine_registration_failure(idx, error))
                    }
                    std::task::Poll::Ready(result) => {
                        self.handles.swap_remove(idx);
                        Some(result)
                    }
                    std::task::Poll::Pending => {
                        // `is_finished` and result publication are expected to
                        // be atomic from the wrapper's perspective, but retain
                        // ownership if an underlying runtime ever exposes a
                        // transient gap. Dropping here would silently detach a
                        // handle whose result was not actually observable.
                        None
                    }
                }
            } else {
                None
            }
        }

        /// Cancel all tasks in the set.
        ///
        /// Signals every task but retains the handles so callers can drain
        /// terminal acknowledgements with `join_next`. A returned aborted
        /// handle means the corresponding task future has been dropped.
        pub fn abort_all(&mut self) {
            for handle in self.handles.iter().chain(&self.unacknowledged) {
                handle.abort();
            }
        }

        #[cfg(test)]
        pub(crate) fn force_join_registration_failure_for_test(&self) {
            self.handles
                .first()
                .expect("JoinSet registration-failure test requires one handle")
                .force_registration_failure_for_test();
        }
    }

    impl<T> Drop for JoinSet<T> {
        fn drop(&mut self) {
            for handle in self.handles.iter().chain(&self.unacknowledged) {
                handle.abort();
            }
        }
    }

    /// Wrapper future that installs the scheduler's current asupersync
    /// `RuntimeHandle` into project thread-local storage before each poll,
    /// enabling nested `task::spawn` calls from within spawned futures.
    ///
    /// The handle is deliberately acquired per poll rather than stored in the
    /// future. A spawned future is owned by `RuntimeInner`; storing a strong
    /// handle here would create a `RuntimeInner -> future -> RuntimeInner`
    /// ownership cycle whenever the future remains pending. Asupersync worker
    /// threads expose a weak current handle specifically so task-local ambient
    /// access cannot keep a replaced or shutting-down runtime alive.
    ///
    /// Visible to the parent module so that `spawn_detached` can also wrap
    /// futures with the correct runtime context.
    pub(super) struct HandleContextFuture<F> {
        /// Runtime-instance shutdown authority. This is an Arc-backed token,
        /// not a runtime handle, so retaining it while Pending cannot keep the
        /// runtime itself alive.
        pub(super) shutdown_token: Option<super::RuntimeShutdownToken>,
        /// Explicit task capability context to expose through `Cx::current()`
        /// for each poll. Plain `task::spawn` leaves this unset so the
        /// scheduler-owned ambient context remains authoritative.
        pub(super) task_cx: Option<crate::cx::Cx>,
        /// Effective capability mask captured once at spawn. A Cx's runtime
        /// mask is stable, so recomputing its public snapshot on every hot
        /// poll would add needless branch and snapshot work.
        pub(super) task_cap_mask: Option<asupersync::cx::CapMask>,
        pub(super) future: Pin<Box<F>>,
    }

    impl<F: Future> Future for HandleContextFuture<F> {
        type Output = F::Output;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let runtime_handle = asupersync::runtime::Runtime::current_handle()
                .expect("runtime task polled without scheduler handle");
            let _runtime_handle_guard = super::install_runtime_handle_scoped(runtime_handle);
            let _runtime_shutdown_guard =
                super::install_runtime_shutdown_token_scoped(self.shutdown_token.clone());
            // asupersync installs a scheduler-owned Cx while polling a task,
            // but `spawn_with_cx` promises that ambient adapters inside the
            // child observe the explicitly threaded context. Install it only
            // for this poll and let the guard restore the scheduler context
            // before returning Pending/Ready.
            let _task_cx_guard = self
                .task_cx
                .as_ref()
                .map(|task_cx| crate::cx::Cx::set_current(Some(task_cx.clone())));
            let _task_capability_guard = self.task_cap_mask.map(crate::cx::Cx::push_restriction);
            self.future.as_mut().poll(cx)
        }
    }

    /// Spawn a future on the current asupersync runtime.
    ///
    /// Uses the thread-local `ASUPERSYNC_HANDLE` installed by
    /// `Runtime::block_on`. Panics if called outside a runtime context.
    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = super::ASUPERSYNC_HANDLE.with(|cell| {
            let borrow = cell.borrow();
            borrow
                .as_ref()
                .cloned()
                .expect("task::spawn called outside of Runtime::block_on context")
        });
        // RuntimeHandle::spawn admits a root scheduler Cx. Capture the ambient
        // context now and reinstall it on every child poll so a plain nested
        // spawn inherits cancellation, deadline, identity, and monotone
        // capability authority rather than escaping into an unrelated root.
        let task_cx = crate::cx::Cx::current();
        let task_cap_mask = task_cx.as_ref().map(crate::cx::effective_cap_mask);
        let wrapped = HandleContextFuture {
            shutdown_token: super::current_runtime_shutdown_token(),
            task_cx,
            task_cap_mask,
            future: Box::pin(future),
        };
        let (abort_handle, abort_registration) = futures::future::AbortHandle::new_pair();
        let inner = handle.spawn(futures::future::Abortable::new(wrapped, abort_registration));
        let (forwarding, completion_waker) = super::ContainedForwardingWaker::new(
            &JOIN_HANDLE_LOCK_POISONED_COUNT,
            &JOIN_HANDLE_WAKER_CALLBACK_PANIC_COUNT,
            frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
        );
        JoinHandle {
            inner: Box::pin(inner),
            abort_handle,
            forwarding,
            completion_waker,
            #[cfg(test)]
            force_registration_failure: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn spawn_with_cx<F, Fut, T>(cx: &crate::cx::Cx, task: F) -> JoinHandle<T>
    where
        F: FnOnce(crate::cx::Cx) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let handle = super::ASUPERSYNC_HANDLE.with(|cell| {
            let borrow = cell.borrow();
            borrow
                .as_ref()
                .cloned()
                .expect("task::spawn_with_cx called outside of Runtime::block_on context")
        });
        let child_cx = cx.clone();
        let child_cap_mask = crate::cx::effective_cap_mask(&child_cx);
        let wrapped = HandleContextFuture {
            shutdown_token: super::current_runtime_shutdown_token(),
            task_cx: Some(child_cx.clone()),
            task_cap_mask: Some(child_cap_mask),
            future: Box::pin(async move { task(child_cx).await }),
        };
        let (abort_handle, abort_registration) = futures::future::AbortHandle::new_pair();
        let inner = handle.spawn(futures::future::Abortable::new(wrapped, abort_registration));
        let (forwarding, completion_waker) = super::ContainedForwardingWaker::new(
            &JOIN_HANDLE_LOCK_POISONED_COUNT,
            &JOIN_HANDLE_WAKER_CALLBACK_PANIC_COUNT,
            frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
        );
        JoinHandle {
            inner: Box::pin(inner),
            abort_handle,
            forwarding,
            completion_waker,
            #[cfg(test)]
            force_registration_failure: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Fallible Cx-aware spawn on the currently installed runtime.
    ///
    /// Unlike [`spawn_with_cx`], this returns a typed admission error instead
    /// of panicking when no runtime is installed or the runtime rejects the
    /// task. Every child poll installs the exact explicit Cx identity and its
    /// captured effective capability mask, including for nested ambient
    /// runtime helpers.
    pub fn try_spawn_with_cx<F, Fut, T>(
        cx: &crate::cx::Cx,
        task: F,
    ) -> Result<JoinHandle<T>, SpawnError>
    where
        F: FnOnce(crate::cx::Cx) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let handle = super::ASUPERSYNC_HANDLE
            .with(|cell| cell.borrow().as_ref().cloned())
            .ok_or(SpawnError::RuntimeUnavailable)?;
        let child_cx = cx.clone();
        let child_cap_mask = crate::cx::effective_cap_mask(&child_cx);
        let wrapped = HandleContextFuture {
            shutdown_token: super::current_runtime_shutdown_token(),
            task_cx: Some(child_cx.clone()),
            task_cap_mask: Some(child_cap_mask),
            future: Box::pin(async move { task(child_cx).await }),
        };
        let (abort_handle, abort_registration) = futures::future::AbortHandle::new_pair();
        let inner =
            handle.try_spawn(futures::future::Abortable::new(wrapped, abort_registration))?;
        let (forwarding, completion_waker) = super::ContainedForwardingWaker::new(
            &JOIN_HANDLE_LOCK_POISONED_COUNT,
            &JOIN_HANDLE_WAKER_CALLBACK_PANIC_COUNT,
            frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
        );
        Ok(JoinHandle {
            inner: Box::pin(inner),
            abort_handle,
            forwarding,
            completion_waker,
            #[cfg(test)]
            force_registration_failure: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Spawns blocking work on the runtime's blocking thread pool.
    ///
    /// Returns a `JoinHandle` for API compatibility. The canonical bridge
    /// selects the installed runtime's configured pool (or a bounded fallback
    /// thread when no pool exists) through an abortable async wrapper.
    ///
    /// Calling [`JoinHandle::abort`] can stop awaiting and delivering the
    /// closure's result, but an OS/blocking closure that has started is not
    /// interruptible and continues until it returns naturally. An aborted join
    /// acknowledgement therefore settles only the async wrapper; shutdown code
    /// must never report that the blocking closure itself stopped unless it has
    /// an independent cooperative-cancellation and settlement contract.
    pub fn spawn_blocking<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let blocking_context = crate::cx::Cx::current().map(|cx| {
            let mask = crate::cx::effective_cap_mask(&cx);
            (cx, mask)
        });
        let blocking_runtime_handle = super::current_runtime_handle();
        // The shared bridge selects the configured pool from the installed
        // runtime handle rather than the operation Cx. This matters for nested
        // explicit-Cx tasks: synthetic/request contexts carry no pool and the
        // raw Asupersync helper would otherwise run `f` inline on the executor.
        spawn(async move {
            super::spawn_blocking_in_context(blocking_context, blocking_runtime_handle, f)
                .await
                .unwrap_or_else(|_| panic!("blocking task failed at canonical runtime boundary"))
        })
    }

    /// Yields execution back to the runtime, allowing other tasks to progress.
    pub async fn yield_now() {
        asupersync::runtime::yield_now().await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`yield_now`].
    ///
    /// Hot-loop cooperative cancellation point: a pre-flight
    /// `cx.checkpoint()` before the runtime yield turns this into
    /// a fast cancellation-sensing yield. The finite `JoinError` return keeps
    /// cancellation-vs-budget identity without exposing a caller-provided
    /// cancellation message.
    pub async fn yield_now_with_cx(cx: &crate::cx::Cx) -> Result<(), JoinError> {
        cx.checkpoint()
            .map_err(|_| JoinError::from_context_failure(cx))?;
        asupersync::runtime::yield_now().await;
        Ok(())
    }
}

/// Yield execution back to the canonical runtime.
pub use task::yield_now;

/// Re-export `join!` macro for concurrent future evaluation.
pub use futures::join;

#[doc(hidden)]
pub mod __select_private {
    pub use futures::future::{Either, select};
    pub use futures::pin_mut;
}

/// Two-branch `select!` macro — polls two futures concurrently and
/// executes the handler of whichever completes first.
///
/// Syntax mirrors `tokio::select!` for the 2-branch case:
/// ```ignore
/// select! {
///     val = future_a => { /* handle val */ }
///     val = future_b => { /* handle val */ }
/// }
/// ```
///
/// Implemented via `futures::future::select` — no tokio dependency.
/// The first branch listed gets a slight bias (left-side of `Either`)
/// but both are polled on every waker notification.
#[macro_export]
macro_rules! select {
    (@poll2 ($pat1:pat, $fut1:expr, $body1:expr) ($pat2:pat, $fut2:expr, $body2:expr)) => {{
        let __ft_select_fut1 = $fut1;
        let __ft_select_fut2 = $fut2;
        $crate::runtime_async::__select_private::pin_mut!(__ft_select_fut1);
        $crate::runtime_async::__select_private::pin_mut!(__ft_select_fut2);
        match $crate::runtime_async::__select_private::select(
            __ft_select_fut1,
            __ft_select_fut2,
        )
        .await
        {
            $crate::runtime_async::__select_private::Either::Left(($pat1, _)) => $body1,
            $crate::runtime_async::__select_private::Either::Right(($pat2, _)) => $body2,
        }
    }};
    (@poll3 ($pat1:pat, $fut1:expr, $body1:expr) ($pat2:pat, $fut2:expr, $body2:expr) ($pat3:pat, $fut3:expr, $body3:expr)) => {{
        let __ft_select_fut1 = $fut1;
        let __ft_select_fut2 = $fut2;
        let __ft_select_fut3 = $fut3;
        $crate::runtime_async::__select_private::pin_mut!(__ft_select_fut1);
        $crate::runtime_async::__select_private::pin_mut!(__ft_select_fut2);
        $crate::runtime_async::__select_private::pin_mut!(__ft_select_fut3);
        let __ft_select_fut23 = $crate::runtime_async::__select_private::select(
            __ft_select_fut2,
            __ft_select_fut3,
        );
        $crate::runtime_async::__select_private::pin_mut!(__ft_select_fut23);
        match $crate::runtime_async::__select_private::select(
            __ft_select_fut1,
            __ft_select_fut23,
        )
        .await
        {
            $crate::runtime_async::__select_private::Either::Left(($pat1, _)) => $body1,
            $crate::runtime_async::__select_private::Either::Right((
                $crate::runtime_async::__select_private::Either::Left(($pat2, _)),
                _,
            )) => $body2,
            $crate::runtime_async::__select_private::Either::Right((
                $crate::runtime_async::__select_private::Either::Right(($pat3, _)),
                _,
            )) => $body3,
        }
    }};
    // Optional tokio-compatible bias marker. The implementation is already
    // left-biased when both branches are ready, matching the two-branch subset
    // this adapter supports.
    (biased; $pat1:pat = $fut1:expr => $body1:expr, $pat2:pat = $fut2:expr => $body2:expr $(,)?) => {{
        $crate::select!(@poll2 ($pat1, $fut1, $body1) ($pat2, $fut2, $body2))
    }};
    // Three-branch, block bodies, no trailing comma between branches.
    ($pat1:pat = $fut1:expr => $body1:block $pat2:pat = $fut2:expr => $body2:block $pat3:pat = $fut3:expr => $body3:block $(,)?) => {{
        $crate::select!(
            @poll3
            ($pat1, $fut1, $body1)
            ($pat2, $fut2, $body2)
            ($pat3, $fut3, $body3)
        )
    }};
    // Three-branch, expression bodies, comma-separated.
    ($pat1:pat = $fut1:expr => $body1:expr, $pat2:pat = $fut2:expr => $body2:expr, $pat3:pat = $fut3:expr => $body3:expr $(,)?) => {{
        $crate::select!(
            @poll3
            ($pat1, $fut1, $body1)
            ($pat2, $fut2, $body2)
            ($pat3, $fut3, $body3)
        )
    }};
    // Two-branch, block bodies, no trailing comma between branches.
    ($pat1:pat = $fut1:expr => $body1:block $pat2:pat = $fut2:expr => $body2:block $(,)?) => {{
        $crate::select!(@poll2 ($pat1, $fut1, $body1) ($pat2, $fut2, $body2))
    }};
    // Two-branch, first branch block body and second branch expression body.
    ($pat1:pat = $fut1:expr => $body1:block $pat2:pat = $fut2:expr => $body2:expr $(,)?) => {{
        $crate::select!(@poll2 ($pat1, $fut1, $body1) ($pat2, $fut2, $body2))
    }};
    // Two-branch, expression bodies, comma-separated
    ($pat1:pat = $fut1:expr => $body1:expr, $pat2:pat = $fut2:expr => $body2:expr $(,)?) => {{
        $crate::select!(@poll2 ($pat1, $fut1, $body1) ($pat2, $fut2, $body2))
    }};
}
pub use crate::select;

/// Unix socket aliases/helpers for the active runtime.
#[cfg(unix)]
pub mod unix {
    use std::io;
    use std::path::Path;

    pub use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
    pub use asupersync::net::{UnixListener, UnixStream};

    pub type LineReader<T> = asupersync::io::Lines<BufReader<T>>;

    pub async fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
        let path = path.as_ref();
        let _ = std::fs::remove_file(path);
        UnixListener::bind(path).await
    }

    pub async fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
        UnixStream::connect(path).await
    }

    #[must_use]
    pub fn buffered<T: AsyncRead>(stream: T) -> BufReader<T> {
        BufReader::new(stream)
    }

    #[must_use]
    pub fn lines<T>(reader: BufReader<T>) -> LineReader<T>
    where
        T: AsyncRead + Unpin,
    {
        asupersync::io::Lines::new(reader)
    }

    /// Line reader with an explicit maximum line length.
    ///
    /// `lines()` inherits asupersync's 64 KiB default cap, which is
    /// SMALLER than the default IPC message limit (128 KiB) — callers
    /// enforcing their own byte budget must pass it explicitly or the
    /// hidden cap fails their reads with `InvalidData` before their
    /// own limit logic ever runs (ft-kccj8).
    #[must_use]
    pub fn lines_with_max_length<T>(reader: BufReader<T>, max_length: usize) -> LineReader<T>
    where
        T: AsyncRead + Unpin,
    {
        asupersync::io::Lines::new_with_max_length(reader, max_length)
    }

    pub async fn next_line<T>(lines: &mut LineReader<T>) -> io::Result<Option<String>>
    where
        T: AsyncRead + Unpin,
    {
        use asupersync::stream::StreamExt;

        match lines.next().await {
            Some(Ok(line)) => Ok(Some(line)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`next_line`]. Performs a pre-flight
    /// `cx.checkpoint()` folded into `io::ErrorKind::Interrupted` so cancelled
    /// line-reading loops bail before the next `lines.next()` poll. The
    /// underlying asupersync stream does not itself observe cx here; this seam
    /// gates entry to the wait.
    pub async fn next_line_with_cx<T>(
        cx: &crate::cx::Cx,
        lines: &mut LineReader<T>,
    ) -> io::Result<Option<String>>
    where
        T: AsyncRead + Unpin,
    {
        use asupersync::stream::StreamExt;

        cx.checkpoint().map_err(|err| {
            io::Error::new(
                io::ErrorKind::Interrupted,
                format!("next_line cancelled: {err}"),
            )
        })?;
        match lines.next().await {
            Some(Ok(line)) => Ok(Some(line)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }
}

/// Unix socket aliases/helpers for the active runtime.
/// Async process primitives routed through the compat boundary.
///
/// This wraps `std::process::Command` and runs blocking process I/O on the
/// runtime's blocking executor so callers can keep a uniform async API
/// without depending on any backend-native process layer directly.
pub mod process {
    use std::ffi::OsStr;
    use std::io::{Read as _, Write as _};
    use std::process::{ExitStatus, Output, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use filedescriptor::{
        AsRawSocketDescriptor, FileDescriptor, POLLIN, POLLOUT, poll, pollfd, socketpair,
    };

    const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
    const PROCESS_POST_EXIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
    const PROCESS_TERMINATION_SETTLE_TIMEOUT: Duration = Duration::from_millis(250);
    const PROCESS_SIGNAL_HELPER_TIMEOUT: Duration = Duration::from_millis(100);
    const PROCESS_SIGNAL_HELPER_REAP_TIMEOUT: Duration = Duration::from_millis(100);
    const PROCESS_SIGNAL_HELPER_TOTAL_TIMEOUT: Duration = Duration::from_millis(200);
    const PROCESS_CAPTURE_READ_CHUNK_BYTES: usize = 16 * 1024;
    const PROCESS_CAPTURE_READ_BYTES_PER_TURN: usize = 256 * 1024;
    const PROCESS_CAPTURE_INITIAL_RESERVE_BYTES: usize = 64 * 1024;
    const PROCESS_INPUT_WRITE_CHUNK_BYTES: usize = 16 * 1024;
    const PROCESS_INPUT_WRITE_BYTES_PER_TURN: usize = 256 * 1024;
    /// Default maximum owned stdin bytes retained by one [`Command`].
    pub const DEFAULT_COMMAND_STDIN_LIMIT_BYTES: usize = 16 * 1024 * 1024;
    /// Default maximum stdout bytes retained by one [`Command::output`] call.
    /// Callers with a narrower or deliberately wider contract should set it
    /// explicitly with [`Command::stdout_limit`].
    pub const DEFAULT_COMMAND_STDOUT_LIMIT_BYTES: usize = 16 * 1024 * 1024;
    /// Default maximum stderr bytes retained by one [`Command::output`] call.
    pub const DEFAULT_COMMAND_STDERR_LIMIT_BYTES: usize = 256 * 1024;
    #[cfg(unix)]
    const UNIX_KILL_COMMANDS: &[&str] = &["/bin/kill", "/usr/bin/kill"];

    /// Output stream whose capture budget was exhausted.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CommandOutputStream {
        Stdout,
        Stderr,
    }

    impl std::fmt::Display for CommandOutputStream {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(match self {
                Self::Stdout => "stdout",
                Self::Stderr => "stderr",
            })
        }
    }

    /// Stable, content-free detail attached to the `io::Error` returned when
    /// a child crosses one of [`Command`]'s output capture limits.
    ///
    /// `observed` is the minimum byte count known when capture stopped; the
    /// child is terminated immediately, so it is intentionally not a claim
    /// about the complete output the child might otherwise have produced.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CommandOutputLimitExceeded {
        stream: CommandOutputStream,
        observed: usize,
        limit: usize,
    }

    impl CommandOutputLimitExceeded {
        #[must_use]
        pub const fn new(stream: CommandOutputStream, observed: usize, limit: usize) -> Self {
            Self {
                stream,
                observed,
                limit,
            }
        }

        #[must_use]
        pub const fn stream(&self) -> CommandOutputStream {
            self.stream
        }

        #[must_use]
        pub const fn observed(&self) -> usize {
            self.observed
        }

        #[must_use]
        pub const fn limit(&self) -> usize {
            self.limit
        }

        /// Recover the stable detail from a command-capture `io::Error`.
        #[must_use]
        pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
            error.get_ref()?.downcast_ref::<Self>()
        }

        #[must_use]
        pub fn into_io_error(self) -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::InvalidData, self)
        }
    }

    impl std::fmt::Display for CommandOutputLimitExceeded {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "process command {} capture limit exceeded: observed at least {} bytes, limit {}",
                self.stream, self.observed, self.limit
            )
        }
    }

    impl std::error::Error for CommandOutputLimitExceeded {}

    /// Stable, content-free detail returned before spawn when configured stdin
    /// exceeds its explicit byte budget.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CommandInputLimitExceeded {
        observed: usize,
        limit: usize,
    }

    impl CommandInputLimitExceeded {
        #[must_use]
        pub const fn observed(&self) -> usize {
            self.observed
        }

        #[must_use]
        pub const fn limit(&self) -> usize {
            self.limit
        }

        #[must_use]
        pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
            error.get_ref()?.downcast_ref::<Self>()
        }

        #[must_use]
        fn into_io_error(self) -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, self)
        }
    }

    impl std::fmt::Display for CommandInputLimitExceeded {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "process command stdin limit exceeded: observed {} bytes, limit {}",
                self.observed, self.limit
            )
        }
    }

    impl std::error::Error for CommandInputLimitExceeded {}

    /// Stable, content-free detail returned when the supervisor cannot deliver
    /// all configured stdin bytes to the child.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CommandInputWriteFailed {
        error_kind: std::io::ErrorKind,
        written: usize,
        total: usize,
    }

    impl CommandInputWriteFailed {
        #[must_use]
        pub const fn error_kind(&self) -> std::io::ErrorKind {
            self.error_kind
        }

        #[must_use]
        pub const fn written(&self) -> usize {
            self.written
        }

        #[must_use]
        pub const fn total(&self) -> usize {
            self.total
        }

        #[must_use]
        pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
            error.get_ref()?.downcast_ref::<Self>()
        }

        #[must_use]
        fn into_io_error(self) -> std::io::Error {
            std::io::Error::new(self.error_kind, self)
        }
    }

    impl std::fmt::Display for CommandInputWriteFailed {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "process command stdin write failed ({:?}, written {} of {} bytes)",
                self.error_kind, self.written, self.total
            )
        }
    }

    impl std::error::Error for CommandInputWriteFailed {}

    /// Stable, content-free failure returned when the child leader exited but
    /// inherited output descriptors did not reach EOF within the bounded drain
    /// window. Returning a partial `Output` as complete would let callers parse
    /// or persist truncated data, so this condition fails closed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CommandOutputCaptureIncomplete {
        stdout_open: bool,
        stderr_open: bool,
        drain_timeout_ms: u64,
    }

    impl CommandOutputCaptureIncomplete {
        #[must_use]
        pub const fn stdout_open(&self) -> bool {
            self.stdout_open
        }

        #[must_use]
        pub const fn stderr_open(&self) -> bool {
            self.stderr_open
        }

        #[must_use]
        pub const fn drain_timeout_ms(&self) -> u64 {
            self.drain_timeout_ms
        }

        #[must_use]
        pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
            error.get_ref()?.downcast_ref::<Self>()
        }

        #[must_use]
        fn into_io_error(self) -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::TimedOut, self)
        }
    }

    /// Convert an owned capture buffer to text without copying valid UTF-8.
    /// Invalid UTF-8 retains the historical lossy replacement behavior and
    /// allocates only on that exceptional path.
    #[must_use]
    pub(crate) fn decode_captured_bytes_lossy(bytes: Vec<u8>) -> String {
        match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
        }
    }

    impl std::fmt::Display for CommandOutputCaptureIncomplete {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "process command output capture incomplete after {} ms (stdout_open={}, stderr_open={})",
                self.drain_timeout_ms, self.stdout_open, self.stderr_open
            )
        }
    }

    impl std::error::Error for CommandOutputCaptureIncomplete {}

    /// Stable, content-free detail attached to an interrupted command error.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CommandCancelled;

    impl CommandCancelled {
        /// Recover the stable detail from a command-capture `io::Error`.
        #[must_use]
        pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
            error.get_ref()?.downcast_ref::<Self>()
        }

        #[must_use]
        fn into_io_error(self) -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::Interrupted, self)
        }
    }

    impl std::fmt::Display for CommandCancelled {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("process command cancelled")
        }
    }

    impl std::error::Error for CommandCancelled {}

    /// Stable, content-free detail attached to a command deadline error.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CommandTimedOut {
        timeout_ms: u64,
    }

    impl CommandTimedOut {
        #[must_use]
        pub const fn timeout_ms(&self) -> u64 {
            self.timeout_ms
        }

        /// Recover the stable detail from a command-capture `io::Error`.
        #[must_use]
        pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
            error.get_ref()?.downcast_ref::<Self>()
        }

        #[must_use]
        fn into_io_error(self) -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::TimedOut, self)
        }
    }

    impl std::fmt::Display for CommandTimedOut {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "process command timed out after {} ms",
                self.timeout_ms
            )
        }
    }

    impl std::error::Error for CommandTimedOut {}

    /// Bounded signal-helper phase that failed to establish a terminal state.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CommandSignalHelperFailurePhase {
        CompletionProbe,
        PostKillProbe,
        PostKillDeadline,
    }

    impl std::fmt::Display for CommandSignalHelperFailurePhase {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(match self {
                Self::CompletionProbe => "completion_probe",
                Self::PostKillProbe => "post_kill_probe",
                Self::PostKillDeadline => "post_kill_deadline",
            })
        }
    }

    /// Content-free proof that a spawned `kill`/`taskkill` helper could not be
    /// confirmed reaped. The helper is never targeted after a non-Interrupted
    /// status-probe error because its numeric identity is then uncertain.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CommandSignalHelperCleanupIncomplete {
        phase: CommandSignalHelperFailurePhase,
        probe_error_kind: Option<std::io::ErrorKind>,
    }

    impl CommandSignalHelperCleanupIncomplete {
        #[must_use]
        pub const fn phase(&self) -> CommandSignalHelperFailurePhase {
            self.phase
        }

        #[must_use]
        pub const fn probe_error_kind(&self) -> Option<std::io::ErrorKind> {
            self.probe_error_kind
        }

        #[must_use]
        pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
            error.get_ref()?.downcast_ref::<Self>()
        }

        #[must_use]
        fn into_io_error(self) -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::TimedOut, self)
        }
    }

    impl std::fmt::Display for CommandSignalHelperCleanupIncomplete {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "process signal helper cleanup incomplete (phase={}, probe_error_kind={:?})",
                self.phase, self.probe_error_kind
            )
        }
    }

    impl std::error::Error for CommandSignalHelperCleanupIncomplete {}

    /// Stable class identifying the failure that initiated bounded process
    /// cleanup. It intentionally carries no program, argument, path, PID, or
    /// child-output content.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CommandCleanupTrigger {
        Cancelled,
        TimedOut,
        CaptureLimit(CommandOutputStream),
        CaptureRead,
        StdinWrite,
        ReadinessPoll,
        StatusProbe,
    }

    impl std::fmt::Display for CommandCleanupTrigger {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Cancelled => formatter.write_str("cancelled"),
                Self::TimedOut => formatter.write_str("timed_out"),
                Self::CaptureLimit(stream) => write!(formatter, "{stream}_capture_limit"),
                Self::CaptureRead => formatter.write_str("capture_read"),
                Self::StdinWrite => formatter.write_str("stdin_write"),
                Self::ReadinessPoll => formatter.write_str("readiness_poll"),
                Self::StatusProbe => formatter.write_str("status_probe"),
            }
        }
    }

    /// Stable, content-free detail returned when bounded cleanup could not
    /// prove leader reap, signal-helper settlement, process-tree signalling,
    /// and inherited capture-descriptor closure before the shared deadline.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CommandProcessCleanupIncomplete {
        trigger: CommandCleanupTrigger,
        leader_reaped: bool,
        signal_helper_settled: bool,
        process_tree_signalled: bool,
        stdout_open: bool,
        stderr_open: bool,
        settle_timeout_ms: u64,
    }

    impl CommandProcessCleanupIncomplete {
        #[must_use]
        pub const fn trigger(&self) -> CommandCleanupTrigger {
            self.trigger
        }

        #[must_use]
        pub const fn leader_reaped(&self) -> bool {
            self.leader_reaped
        }

        #[must_use]
        pub const fn signal_helper_settled(&self) -> bool {
            self.signal_helper_settled
        }

        #[must_use]
        pub const fn process_tree_signalled(&self) -> bool {
            self.process_tree_signalled
        }

        #[must_use]
        pub const fn stdout_open(&self) -> bool {
            self.stdout_open
        }

        #[must_use]
        pub const fn stderr_open(&self) -> bool {
            self.stderr_open
        }

        #[must_use]
        pub const fn settle_timeout_ms(&self) -> u64 {
            self.settle_timeout_ms
        }

        /// Recover the stable detail from a command-supervisor `io::Error`.
        #[must_use]
        pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
            error.get_ref()?.downcast_ref::<Self>()
        }

        #[must_use]
        fn into_io_error(self) -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::TimedOut, self)
        }
    }

    impl std::fmt::Display for CommandProcessCleanupIncomplete {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "process command cleanup incomplete after {} ms (trigger={}, leader_reaped={}, signal_helper_settled={}, process_tree_signalled={}, stdout_open={}, stderr_open={})",
                self.settle_timeout_ms,
                self.trigger,
                self.leader_reaped,
                self.signal_helper_settled,
                self.process_tree_signalled,
                self.stdout_open,
                self.stderr_open
            )
        }
    }

    impl std::error::Error for CommandProcessCleanupIncomplete {}

    /// Cloneable cancellation signal for synchronous command supervision.
    #[derive(Debug, Clone, Default)]
    pub struct CommandCancellation {
        requested: Arc<AtomicBool>,
    }

    impl CommandCancellation {
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Request cancellation. Repeated calls are idempotent.
        pub fn cancel(&self) {
            self.requested.store(true, Ordering::SeqCst);
        }

        #[must_use]
        pub fn is_cancelled(&self) -> bool {
            self.requested.load(Ordering::SeqCst)
        }

        fn shared_flag(&self) -> Arc<AtomicBool> {
            Arc::clone(&self.requested)
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct OutputCaptureLimits {
        stdout: usize,
        stderr: usize,
    }

    struct OutputCommandSpec {
        program: std::ffi::OsString,
        args: Vec<std::ffi::OsString>,
        envs: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        current_dir: Option<std::path::PathBuf>,
        stdin: Option<Arc<[u8]>>,
        stdin_configuration_error: Option<CommandInputLimitExceeded>,
        stdin_limit: usize,
        limits: OutputCaptureLimits,
        exec_busy_retry_delays: Vec<Duration>,
    }

    #[derive(Debug, Clone, Copy)]
    struct OutputCommandDeadline {
        at: Instant,
        timeout_ms: u64,
    }

    impl OutputCommandDeadline {
        fn new(timeout: Duration) -> Self {
            Self {
                at: process_deadline_after(timeout),
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            }
        }

        fn is_elapsed(self) -> bool {
            Instant::now() >= self.at
        }

        fn remaining(self) -> Duration {
            self.at.saturating_duration_since(Instant::now())
        }

        fn into_io_error(self) -> std::io::Error {
            CommandTimedOut {
                timeout_ms: self.timeout_ms,
            }
            .into_io_error()
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CaptureDrainState {
        Eof,
        Pending,
        QuantumExhausted,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum OutputChildProbeErrorAction {
        RetryUntilDeadline,
        StopWithUncertainIdentity,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct OutputCleanupState {
        leader_reaped: bool,
        signal_helper_settled: bool,
        process_tree_signalled: bool,
        stdout_open: bool,
        stderr_open: bool,
    }

    struct CommandInputState {
        writer: FileDescriptor,
        bytes: Arc<[u8]>,
        written: usize,
    }

    /// Async-compatible process command wrapper backed by `std::process::Command`.
    ///
    /// Mirrors the subset of the legacy compat command surface used by callers:
    /// command/env construction, kill-on-drop, bounded owned input, bounded
    /// output configuration, and async output collection.
    pub struct Command {
        inner: std::process::Command,
        kill_on_drop: bool,
        stdin: Option<Arc<[u8]>>,
        stdin_configuration_error: Option<CommandInputLimitExceeded>,
        stdin_limit: usize,
        stdout_limit: usize,
        stderr_limit: usize,
        exec_busy_retry_delays: Vec<Duration>,
    }

    struct KillOnDropGuard {
        cancel: Arc<AtomicBool>,
        enabled: bool,
    }

    struct PlatformProcessControl;

    trait ProcessControl {
        fn configure_process_group(cmd: &mut std::process::Command) -> std::io::Result<()>;
        fn send_signal_to_pid(
            pid: i64,
            signal: &str,
            deadline: Instant,
        ) -> std::io::Result<ExitStatus>;
        fn send_signal_to_process_group(
            process_group_id: u32,
            signal: &str,
            deadline: Instant,
        ) -> std::io::Result<ExitStatus>;
    }

    #[cfg(unix)]
    pub(crate) fn unix_kill_command() -> &'static str {
        UNIX_KILL_COMMANDS
            .iter()
            .copied()
            .find(|path| std::path::Path::new(path).is_file())
            .unwrap_or(UNIX_KILL_COMMANDS[0])
    }

    #[cfg(unix)]
    fn validate_unix_signal_target(pid: i64) -> std::io::Result<()> {
        if pid <= 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("signal target pid must be positive, got {pid}"),
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn validate_unix_signal_name(signal: &str) -> std::io::Result<()> {
        if !signal.is_empty() && signal.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Ok(());
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid Unix signal name: {signal:?}"),
        ))
    }

    enum SignalHelperReapOutcome {
        Reaped(ExitStatus),
        DeadlineElapsed,
        ProbeUncertain(std::io::ErrorKind),
    }

    fn reap_signal_helper_until(
        helper: &mut std::process::Child,
        deadline: Instant,
    ) -> SignalHelperReapOutcome {
        loop {
            match helper.try_wait() {
                Ok(Some(status)) => return SignalHelperReapOutcome::Reaped(status),
                Ok(None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return SignalHelperReapOutcome::ProbeUncertain(error.kind());
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return SignalHelperReapOutcome::DeadlineElapsed;
            }
            std::thread::sleep(remaining.min(PROCESS_POLL_INTERVAL));
        }
    }

    /// Run the small platform signal helper without ever blocking on
    /// `Child::wait`/`Command::status`. A wedged `kill`/`taskkill` helper is
    /// terminated through its owned child handle and given one final finite
    /// reap window, so process-command cleanup cannot inherit an unbounded
    /// external wait.
    fn run_signal_helper(
        command: &mut std::process::Command,
        overall_deadline: Instant,
    ) -> std::io::Result<ExitStatus> {
        if Instant::now() >= overall_deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "process signal helper deadline elapsed before spawn",
            ));
        }
        let mut helper = command.spawn()?;
        let completion_deadline =
            process_deadline_after(PROCESS_SIGNAL_HELPER_TIMEOUT).min(overall_deadline);
        match reap_signal_helper_until(&mut helper, completion_deadline) {
            SignalHelperReapOutcome::Reaped(status) => return Ok(status),
            SignalHelperReapOutcome::ProbeUncertain(kind) => {
                return Err(CommandSignalHelperCleanupIncomplete {
                    phase: CommandSignalHelperFailurePhase::CompletionProbe,
                    probe_error_kind: Some(kind),
                }
                .into_io_error());
            }
            SignalHelperReapOutcome::DeadlineElapsed => {}
        }

        let _ = helper.kill();
        let reap_deadline =
            process_deadline_after(PROCESS_SIGNAL_HELPER_REAP_TIMEOUT).min(overall_deadline);
        match reap_signal_helper_until(&mut helper, reap_deadline) {
            SignalHelperReapOutcome::Reaped(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "process signal helper timed out",
            )),
            SignalHelperReapOutcome::ProbeUncertain(kind) => {
                Err(CommandSignalHelperCleanupIncomplete {
                    phase: CommandSignalHelperFailurePhase::PostKillProbe,
                    probe_error_kind: Some(kind),
                }
                .into_io_error())
            }
            SignalHelperReapOutcome::DeadlineElapsed => Err(CommandSignalHelperCleanupIncomplete {
                phase: CommandSignalHelperFailurePhase::PostKillDeadline,
                probe_error_kind: None,
            }
            .into_io_error()),
        }
    }

    #[cfg(unix)]
    impl ProcessControl for PlatformProcessControl {
        fn configure_process_group(cmd: &mut std::process::Command) -> std::io::Result<()> {
            use std::os::unix::process::CommandExt;

            cmd.process_group(0);
            Ok(())
        }

        fn send_signal_to_pid(
            pid: i64,
            signal: &str,
            deadline: Instant,
        ) -> std::io::Result<ExitStatus> {
            validate_unix_signal_target(pid)?;
            validate_unix_signal_name(signal)?;

            let mut command = std::process::Command::new(unix_kill_command());
            command
                .args(["-s", signal, &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            run_signal_helper(&mut command, deadline)
        }

        fn send_signal_to_process_group(
            process_group_id: u32,
            signal: &str,
            deadline: Instant,
        ) -> std::io::Result<ExitStatus> {
            validate_unix_signal_target(i64::from(process_group_id))?;
            validate_unix_signal_name(signal)?;

            let mut command = std::process::Command::new(unix_kill_command());
            command
                // A negative numeric target denotes a process group, but GNU
                // `kill` otherwise parses it as an option.  The explicit
                // option terminator is therefore part of the cross-platform
                // command contract, not cosmetic argv formatting.
                .args(["-s", signal, "--", &format!("-{process_group_id}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            run_signal_helper(&mut command, deadline)
        }
    }

    #[cfg(windows)]
    impl ProcessControl for PlatformProcessControl {
        fn configure_process_group(_cmd: &mut std::process::Command) -> std::io::Result<()> {
            Ok(())
        }

        fn send_signal_to_pid(
            pid: i64,
            signal: &str,
            deadline: Instant,
        ) -> std::io::Result<ExitStatus> {
            let pid = validate_windows_signal_target(pid)?;
            run_taskkill(pid, windows_taskkill_force(signal)?, deadline)
        }

        fn send_signal_to_process_group(
            process_group_id: u32,
            signal: &str,
            deadline: Instant,
        ) -> std::io::Result<ExitStatus> {
            validate_windows_signal_target(i64::from(process_group_id))?;
            run_taskkill(process_group_id, windows_taskkill_force(signal)?, deadline)
        }
    }

    #[cfg(not(any(unix, windows)))]
    impl ProcessControl for PlatformProcessControl {
        fn configure_process_group(_cmd: &mut std::process::Command) -> std::io::Result<()> {
            Ok(())
        }

        fn send_signal_to_pid(
            _pid: i64,
            _signal: &str,
            _deadline: Instant,
        ) -> std::io::Result<ExitStatus> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "process signaling is unsupported on this platform",
            ))
        }

        fn send_signal_to_process_group(
            _process_group_id: u32,
            _signal: &str,
            _deadline: Instant,
        ) -> std::io::Result<ExitStatus> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "process-tree signaling is unsupported on this platform",
            ))
        }
    }

    #[cfg(windows)]
    fn validate_windows_signal_target(pid: i64) -> std::io::Result<u32> {
        u32::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("signal target pid must be a positive Windows process id, got {pid}"),
                )
            })
    }

    #[cfg(windows)]
    fn windows_taskkill_force(signal: &str) -> std::io::Result<bool> {
        if signal.is_empty() || !signal.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid Windows termination signal: {signal:?}"),
            ));
        }

        let upper = signal.to_ascii_uppercase();
        let normalized = upper.strip_prefix("SIG").unwrap_or(&upper);
        match normalized {
            "TERM" => Ok(false),
            "KILL" => Ok(true),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unsupported Windows termination signal: {signal:?}"),
            )),
        }
    }

    #[cfg(windows)]
    fn run_taskkill(pid: u32, force: bool, deadline: Instant) -> std::io::Result<ExitStatus> {
        let pid_arg = pid.to_string();
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/PID", &pid_arg, "/T"]);
        if force {
            cmd.arg("/F");
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        run_signal_helper(&mut cmd, deadline)
    }

    /// Configure a freshly built command so its subprocess tree can be
    /// terminated through [`send_signal_to_process_group`].
    ///
    /// Call this before `spawn`; the child PID is then the process-group/tree
    /// identifier accepted by the signaling helpers below.
    pub fn configure_process_group(cmd: &mut std::process::Command) -> std::io::Result<()> {
        PlatformProcessControl::configure_process_group(cmd)
    }

    pub fn send_signal_to_pid(pid: i64, signal: &str) -> std::io::Result<ExitStatus> {
        PlatformProcessControl::send_signal_to_pid(
            pid,
            signal,
            process_deadline_after(PROCESS_SIGNAL_HELPER_TOTAL_TIMEOUT),
        )
    }

    pub fn send_signal_to_process_group(
        process_group_id: u32,
        signal: &str,
    ) -> std::io::Result<ExitStatus> {
        send_signal_to_process_group_until(
            process_group_id,
            signal,
            process_deadline_after(PROCESS_SIGNAL_HELPER_TOTAL_TIMEOUT),
        )
    }

    fn send_signal_to_process_group_until(
        process_group_id: u32,
        signal: &str,
        deadline: Instant,
    ) -> std::io::Result<ExitStatus> {
        PlatformProcessControl::send_signal_to_process_group(process_group_id, signal, deadline)
    }

    /// Send a process-termination signal by pid. Cross-platform: on Unix it
    /// shells out to `kill -s <signal>`, on Windows `TERM`/`KILL` map to
    /// `taskkill` (see the `PlatformProcessControl` impls). The historical
    /// `unix` name is retained for caller stability; it is a direct delegate to
    /// the platform-neutral [`send_signal_to_pid`], not a compat shim.
    pub fn send_unix_signal_to_pid(pid: i64, signal: &str) -> std::io::Result<ExitStatus> {
        send_signal_to_pid(pid, signal)
    }

    /// Send a process-group termination signal. Cross-platform delegate to
    /// [`send_signal_to_process_group`]; see [`send_unix_signal_to_pid`].
    pub fn send_unix_signal_to_process_group(
        process_group_id: u32,
        signal: &str,
    ) -> std::io::Result<ExitStatus> {
        send_signal_to_process_group(process_group_id, signal)
    }

    impl KillOnDropGuard {
        fn new(cancel: Arc<AtomicBool>, enabled: bool) -> Self {
            Self { cancel, enabled }
        }

        fn disarm(&mut self) {
            self.enabled = false;
        }
    }

    impl Drop for KillOnDropGuard {
        fn drop(&mut self) {
            if self.enabled {
                self.cancel.store(true, Ordering::SeqCst);
            }
        }
    }

    /// br-ft-xffjo: RAII signal that the cx-watcher task in
    /// [`Command::output_with_cx`] should exit. The previous
    /// implementation set `watcher_done` manually after the
    /// `spawn_blocking(...).await?` line, which the `?` early-exit
    /// bypassed on JoinError, leaking the watcher task until the
    /// caller's cx eventually cancelled. RAII closes that gap —
    /// the guard's `Drop` always sets the flag, even on `?`,
    /// `return`, or panic in the surrounding function body.
    struct WatcherDoneGuard {
        done: Arc<AtomicBool>,
    }

    impl WatcherDoneGuard {
        fn new(done: Arc<AtomicBool>) -> Self {
            Self { done }
        }
    }

    impl Drop for WatcherDoneGuard {
        fn drop(&mut self) {
            self.done.store(true, Ordering::SeqCst);
        }
    }

    impl Command {
        pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
            Self {
                inner: std::process::Command::new(program),
                kill_on_drop: false,
                stdin: None,
                stdin_configuration_error: None,
                stdin_limit: DEFAULT_COMMAND_STDIN_LIMIT_BYTES,
                stdout_limit: DEFAULT_COMMAND_STDOUT_LIMIT_BYTES,
                stderr_limit: DEFAULT_COMMAND_STDERR_LIMIT_BYTES,
                exec_busy_retry_delays: Vec::new(),
            }
        }

        pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
            self.inner.arg(arg);
            self
        }

        pub fn args<I, S>(&mut self, args: I) -> &mut Self
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            self.inner.args(args);
            self
        }

        pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
        where
            K: AsRef<OsStr>,
            V: AsRef<OsStr>,
        {
            self.inner.env(key, val);
            self
        }

        /// Remove one inherited environment variable from the child.
        ///
        /// This is intentionally narrower than `env_clear`: subprocesses that
        /// need a hermetic capability can suppress injection-specific knobs
        /// while retaining the platform environment required by the child.
        pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Self {
            self.inner.env_remove(key);
            self
        }

        /// Set the working directory used for the child process.
        pub fn current_dir<P: AsRef<std::path::Path>>(&mut self, dir: P) -> &mut Self {
            self.inner.current_dir(dir);
            self
        }

        /// Configure an owned stdin payload. The bytes are delivered through
        /// the same cancellation/deadline-aware nonblocking supervisor as
        /// output capture; no payload content enters argv, environment, or a
        /// temporary file. Without this call, stdin remains null.
        pub fn stdin_bytes<B: Into<Vec<u8>>>(&mut self, bytes: B) -> &mut Self {
            let bytes = bytes.into();
            if bytes.len() > self.stdin_limit {
                self.stdin = None;
                self.stdin_configuration_error = Some(CommandInputLimitExceeded {
                    observed: bytes.len(),
                    limit: self.stdin_limit,
                });
            } else {
                self.stdin = Some(Arc::from(bytes));
                self.stdin_configuration_error = None;
            }
            self
        }

        /// Set the maximum configured stdin payload size. The limit is checked
        /// before descriptors are allocated or a child is spawned.
        pub fn stdin_limit(&mut self, limit: usize) -> &mut Self {
            self.stdin_limit = limit;
            let oversized_observed = self
                .stdin
                .as_ref()
                .map(|bytes| bytes.len())
                .filter(|observed| *observed > self.stdin_limit);
            if let Some(observed) = oversized_observed {
                self.stdin = None;
                self.stdin_configuration_error = Some(CommandInputLimitExceeded {
                    observed,
                    limit: self.stdin_limit,
                });
            }
            self
        }

        /// When enabled, dropping the future returned by [`output`] will signal
        /// the worker loop to terminate the child process promptly.
        pub fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
            self.kill_on_drop = kill;
            self
        }

        /// Set the maximum number of stdout bytes retained by [`Self::output`]
        /// and [`Self::output_with_cx`]. A zero limit permits only empty
        /// stdout. Crossing the limit terminates the child and returns an
        /// `io::Error` carrying [`CommandOutputLimitExceeded`].
        pub fn stdout_limit(&mut self, limit: usize) -> &mut Self {
            self.stdout_limit = limit;
            self
        }

        /// Set the maximum number of stderr bytes retained by [`Self::output`]
        /// and [`Self::output_with_cx`]. A zero limit permits only empty
        /// stderr.
        pub fn stderr_limit(&mut self, limit: usize) -> &mut Self {
            self.stderr_limit = limit;
            self
        }

        /// Configure bounded retry delays for transient `ETXTBSY` spawn
        /// failures. This is crate-internal because only integrations that can
        /// be replaced in place need the write-then-exec race workaround.
        #[cfg(any(feature = "subprocess-bridge", test))]
        pub(crate) fn exec_busy_retry_delays(&mut self, delays: &[Duration]) -> &mut Self {
            self.exec_busy_retry_delays.clear();
            self.exec_busy_retry_delays.extend_from_slice(delays);
            self
        }

        /// Execute synchronously under a finite wall-clock deadline using the
        /// same bounded nonblocking capture supervisor as [`Self::output`].
        pub fn output_blocking(&mut self, timeout: Duration) -> std::io::Result<Output> {
            let cancellation = CommandCancellation::new();
            self.output_blocking_with_cancellation(timeout, &cancellation)
        }

        /// Synchronous bounded output collection with cooperative external
        /// cancellation. Cancellation and timeout errors retain stable typed,
        /// content-free details for callers to classify.
        pub fn output_blocking_with_cancellation(
            &mut self,
            timeout: Duration,
            cancellation: &CommandCancellation,
        ) -> std::io::Result<Output> {
            run_output_command(
                self.output_spec(),
                cancellation.shared_flag(),
                Some(OutputCommandDeadline::new(timeout)),
            )
        }

        /// Executes the command and collects its output, running the blocking
        /// I/O on the runtime's blocking thread pool.
        pub async fn output(&mut self) -> std::io::Result<Output> {
            // Build a fresh std::process::Command to move into the closure
            // (std::process::Command is not Send, so we serialize the config).
            let spec = self.output_spec();
            let cancel = Arc::new(AtomicBool::new(false));
            let mut kill_guard = KillOnDropGuard::new(Arc::clone(&cancel), self.kill_on_drop);

            let result = super::spawn_blocking(move || run_output_command(spec, cancel, None))
                .await
                .map_err(std::io::Error::other)?;

            kill_guard.disarm();
            result
        }

        /// ft-xbnl0.2.3 Cx-first sibling of [`Command::output`].
        ///
        /// Pre-flight `cx.checkpoint()` gates spawn, and a dedicated
        /// watcher task bridges caller-cx cancellation into the
        /// already-existing `cancel: Arc<AtomicBool>` signal that the
        /// `run_output_command` worker polls at
        /// `PROCESS_POLL_INTERVAL`. The result: a caller cancelling
        /// its cx surfaces as `io::ErrorKind::Interrupted` within
        /// ~10ms of the signal (one PROCESS_POLL_INTERVAL tick),
        /// including for long-running child processes that would
        /// otherwise block the spawn_blocking indefinitely. Finite Cx
        /// deadlines and watcher-timer failures take the same fail-closed
        /// child-cancellation path.
        ///
        /// Kill-on-drop semantics are preserved: if the future is
        /// dropped before completion and kill_on_drop was set, the
        /// KillOnDropGuard still fires the cancel flag. The cx
        /// watcher sets `watcher_done` on normal-path exit so it
        /// never leaks past the body.
        pub async fn output_with_cx(&mut self, cx: &crate::cx::Cx) -> std::io::Result<Output> {
            self.output_with_cx_deadline(cx, None).await
        }

        /// Executes the command under both caller-Cx cancellation and a finite
        /// wall-clock deadline.
        ///
        /// Unlike wrapping [`Self::output_with_cx`] in an outer timeout, this
        /// deadline is owned by the blocking process supervisor itself. A
        /// timeout therefore does not return merely because the async future
        /// was dropped: the supervisor terminates the process group, drains
        /// its bounded pipes, and reaps the leader before this method settles.
        /// Caller-Cx cancellation retains the same settled-cleanup guarantee.
        pub async fn output_with_cx_timeout(
            &mut self,
            cx: &crate::cx::Cx,
            timeout: Duration,
        ) -> std::io::Result<Output> {
            self.output_with_cx_deadline(cx, Some(OutputCommandDeadline::new(timeout)))
                .await
        }

        async fn output_with_cx_deadline(
            &mut self,
            cx: &crate::cx::Cx,
            deadline: Option<OutputCommandDeadline>,
        ) -> std::io::Result<Output> {
            cx.checkpoint()
                .map_err(|_| CommandCancelled.into_io_error())?;

            let spec = self.output_spec();
            let cancel = Arc::new(AtomicBool::new(false));
            let mut kill_guard = KillOnDropGuard::new(Arc::clone(&cancel), self.kill_on_drop);

            // Spawn cx→AtomicBool bridge watcher. It polls cx at
            // PROCESS_POLL_INTERVAL and sets `cancel` on cx cancel.
            // It also exits when `watcher_done` is set (normal path).
            //
            // Tick 205 (ft-xbnl0.2.3): the inter-poll sleep now uses
            // sleep_with_cx(&watcher_cx, ...) so both the cancel check
            // and the sleep timer observe the same cx. Previously used
            // ambient super::sleep which falls back to Cx::current()
            // thread-local — asymmetric cx ownership where the watcher
            // checks watcher_cx for cancel but times out via ambient.
            let watcher_done = Arc::new(AtomicBool::new(false));
            let watcher_cancel = Arc::clone(&cancel);
            let watcher_done_inner = Arc::clone(&watcher_done);
            let watcher_cx = cx.clone();
            let watcher_spawn_cx = watcher_cx.clone();
            let watcher_handle =
                super::task::spawn_with_cx(&watcher_spawn_cx, move |_child_cx| async move {
                    while !watcher_done_inner.load(Ordering::SeqCst) {
                        if watcher_cx.checkpoint().is_err() {
                            watcher_cancel.store(true, Ordering::SeqCst);
                            return;
                        }
                        if super::sleep_with_cx(&watcher_cx, PROCESS_POLL_INTERVAL)
                            .await
                            .is_err()
                        {
                            // A finite budget can wake `sleep_with_cx` with an
                            // error before the Cx cancellation bit is latched.
                            // Fail closed and stop the child instead of
                            // spinning this watcher without yielding.
                            let _ = watcher_cx.checkpoint();
                            watcher_cancel.store(true, Ordering::SeqCst);
                            return;
                        }
                    }
                });

            // br-ft-xffjo: RAII guard ensures `watcher_done` is set
            // on every exit path (normal return, `?` early-exit on
            // spawn_blocking JoinError, panic). Without this, a
            // JoinError early-exit leaked the watcher task until
            // the caller's cx cancelled — bounded but real on
            // long-lived cxs.
            let _watcher_done_guard = WatcherDoneGuard::new(Arc::clone(&watcher_done));

            let result =
                super::spawn_blocking(move || run_output_command(spec, cancel, deadline))
                    .await
                    .map_err(std::io::Error::other)?;

            // Drain the watcher on the normal path so it exits
            // before this function returns. The guard above already
            // signalled it via Drop on early-exit paths; here we
            // explicitly signal + drain so the watcher task is
            // joined (not just abandoned) when the function
            // returns Ok. The `_watcher_done_guard` Drop on
            // function exit re-stores `true` (idempotent — no-op
            // since we just set it), then the guard goes out of
            // scope cleanly.
            watcher_done.store(true, Ordering::SeqCst);
            let _ = watcher_handle.await;

            kill_guard.disarm();
            result
        }
    }

    impl Command {
        fn output_spec(&self) -> OutputCommandSpec {
            OutputCommandSpec {
                program: self.inner.get_program().to_os_string(),
                args: self
                    .inner
                    .get_args()
                    .map(|arg| arg.to_os_string())
                    .collect(),
                envs: self
                    .inner
                    .get_envs()
                    .filter_map(|(key, value)| {
                        value.map(|value| (key.to_os_string(), value.to_os_string()))
                    })
                    .collect(),
                current_dir: self
                    .inner
                    .get_current_dir()
                    .map(std::path::Path::to_path_buf),
                stdin: self.stdin.clone(),
                stdin_configuration_error: self.stdin_configuration_error,
                stdin_limit: self.stdin_limit,
                limits: OutputCaptureLimits {
                    stdout: self.stdout_limit,
                    stderr: self.stderr_limit,
                },
                exec_busy_retry_delays: self.exec_busy_retry_delays.clone(),
            }
        }
    }

    fn run_output_command(
        spec: OutputCommandSpec,
        cancel: Arc<AtomicBool>,
        deadline: Option<OutputCommandDeadline>,
    ) -> std::io::Result<Output> {
        // A kill-on-drop future can be cancelled while queued for the blocking
        // pool. Observe that state before allocating descriptors or spawning a
        // process, so a cancelled operation cannot launch after its caller has
        // already gone away.
        check_output_command_abort(&cancel, deadline)?;

        let OutputCommandSpec {
            program,
            args,
            envs,
            current_dir,
            stdin,
            stdin_configuration_error,
            stdin_limit,
            limits,
            exec_busy_retry_delays,
        } = spec;

        if let Some(error) = stdin_configuration_error {
            return Err(error.into_io_error());
        }
        validate_command_input(stdin.as_deref(), stdin_limit)?;

        let (mut stdout_read, stdout_write) = process_capture_socketpair("stdout")?;
        let (mut stderr_read, stderr_write) = process_capture_socketpair("stderr")?;
        let (mut stdin_write, stdin_read) = if stdin.is_some() {
            let (write, read) = process_capture_socketpair("stdin")?;
            (Some(write), Some(read))
        } else {
            (None, None)
        };
        stdout_read
            .set_non_blocking(true)
            .map_err(|error| process_capture_setup_error("stdout nonblocking mode", error))?;
        stderr_read
            .set_non_blocking(true)
            .map_err(|error| process_capture_setup_error("stderr nonblocking mode", error))?;
        if let Some(writer) = stdin_write.as_mut() {
            writer
                .set_non_blocking(true)
                .map_err(|error| process_capture_setup_error("stdin nonblocking mode", error))?;
        }

        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        if let Some(stdin_read) = stdin_read.as_ref() {
            cmd.stdin(
                stdin_read
                    .as_stdio()
                    .map_err(|error| process_capture_setup_error("stdin child handle", error))?,
            );
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(
            stdout_write
                .as_stdio()
                .map_err(|error| process_capture_setup_error("stdout child handle", error))?,
        );
        cmd.stderr(
            stderr_write
                .as_stdio()
                .map_err(|error| process_capture_setup_error("stderr child handle", error))?,
        );
        for (k, v) in envs {
            cmd.env(k, v);
        }
        if let Some(current_dir) = current_dir {
            cmd.current_dir(current_dir);
        }

        configure_process_group(&mut cmd)?;

        // Close the remaining queue/setup window. Cancellation can still race
        // the spawn itself, but the first worker-loop iteration will then
        // terminate that process tree promptly.
        check_output_command_abort(&cancel, deadline)?;

        let mut child = spawn_output_child(&mut cmd, &exec_busy_retry_delays, &cancel, deadline)?;
        // `Command` retains custom Stdio handles for possible reuse. Drop it
        // and our original write endpoints immediately so only the child (or
        // descendants that deliberately inherit them) can keep either stream
        // open. This is essential for reliable EOF detection.
        drop(cmd);
        drop(stdin_read);
        drop(stdout_write);
        drop(stderr_write);

        let mut stdin_state = stdin
            .zip(stdin_write)
            .map(|(bytes, writer)| CommandInputState {
                writer,
                bytes,
                written: 0,
            });
        let mut stdout_reader = Some(stdout_read);
        let mut stderr_reader = Some(stderr_read);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut status = None;
        let mut post_exit_deadline = None;

        loop {
            if cancel.load(Ordering::SeqCst) {
                terminate_and_settle_output_command(
                    &mut child,
                    status.is_some(),
                    CommandCleanupTrigger::Cancelled,
                    &mut stdin_state,
                    &mut stdout_reader,
                    &mut stderr_reader,
                    &mut stdout,
                    &mut stderr,
                    limits,
                )?;
                return Err(process_command_cancelled_error());
            }
            if let Some(deadline) = deadline
                && deadline.is_elapsed()
            {
                terminate_and_settle_output_command(
                    &mut child,
                    status.is_some(),
                    CommandCleanupTrigger::TimedOut,
                    &mut stdin_state,
                    &mut stdout_reader,
                    &mut stderr_reader,
                    &mut stdout,
                    &mut stderr,
                    limits,
                )?;
                return Err(deadline.into_io_error());
            }

            let stdin_quantum_exhausted = match write_command_input(&mut stdin_state) {
                Ok(quantum_exhausted) => quantum_exhausted,
                Err(error) => {
                    terminate_and_settle_output_command(
                        &mut child,
                        status.is_some(),
                        CommandCleanupTrigger::StdinWrite,
                        &mut stdin_state,
                        &mut stdout_reader,
                        &mut stderr_reader,
                        &mut stdout,
                        &mut stderr,
                        limits,
                    )?;
                    return Err(error);
                }
            };

            let stdout_quantum_exhausted = match drain_capture_reader(
                &mut stdout_reader,
                &mut stdout,
                limits.stdout,
                CommandOutputStream::Stdout,
            ) {
                Ok(quantum_exhausted) => quantum_exhausted,
                Err(error) => {
                    terminate_and_settle_output_command(
                        &mut child,
                        status.is_some(),
                        output_cleanup_trigger_for_capture_error(&error),
                        &mut stdin_state,
                        &mut stdout_reader,
                        &mut stderr_reader,
                        &mut stdout,
                        &mut stderr,
                        limits,
                    )?;
                    return Err(error);
                }
            };
            let stderr_quantum_exhausted = match drain_capture_reader(
                &mut stderr_reader,
                &mut stderr,
                limits.stderr,
                CommandOutputStream::Stderr,
            ) {
                Ok(quantum_exhausted) => quantum_exhausted,
                Err(error) => {
                    terminate_and_settle_output_command(
                        &mut child,
                        status.is_some(),
                        output_cleanup_trigger_for_capture_error(&error),
                        &mut stdin_state,
                        &mut stdout_reader,
                        &mut stderr_reader,
                        &mut stdout,
                        &mut stderr,
                        limits,
                    )?;
                    return Err(error);
                }
            };
            let io_quantum_exhausted =
                stdin_quantum_exhausted || stdout_quantum_exhausted || stderr_quantum_exhausted;

            if status.is_none() {
                match child.try_wait() {
                    Ok(Some(exit_status)) => {
                        if let Some(input) = stdin_state.take() {
                            let error = CommandInputWriteFailed {
                                error_kind: std::io::ErrorKind::BrokenPipe,
                                written: input.written,
                                total: input.bytes.len(),
                            }
                            .into_io_error();
                            terminate_and_settle_output_command(
                                &mut child,
                                true,
                                CommandCleanupTrigger::StdinWrite,
                                &mut stdin_state,
                                &mut stdout_reader,
                                &mut stderr_reader,
                                &mut stdout,
                                &mut stderr,
                                limits,
                            )?;
                            return Err(error);
                        }
                        status = Some(exit_status);
                        post_exit_deadline =
                            Some(process_deadline_after(PROCESS_POST_EXIT_DRAIN_TIMEOUT));
                    }
                    Ok(None) => {}
                    Err(error)
                        if output_child_probe_error_action(&error)
                            == OutputChildProbeErrorAction::RetryUntilDeadline =>
                    {
                        // Return to the deadline/cancellation-aware outer
                        // loop. Readiness polling below bounds the retry rate,
                        // so repeated signals cannot create a hot spin.
                    }
                    Err(error) => {
                        // The state probe failed, so the numeric process/group
                        // identity is not safe to signal. Fail closed: close
                        // capture within the bounded settlement window and
                        // preserve the probe error for the caller.
                        let settlement_deadline =
                            process_deadline_after(PROCESS_TERMINATION_SETTLE_TIMEOUT);
                        stdin_state.take();
                        let cleanup = settle_output_command(
                            &mut child,
                            false,
                            true,
                            false,
                            &mut stdout_reader,
                            &mut stderr_reader,
                            &mut stdout,
                            &mut stderr,
                            limits,
                            settlement_deadline,
                        );
                        ensure_output_cleanup_complete(
                            CommandCleanupTrigger::StatusProbe,
                            cleanup,
                        )?;
                        return Err(error);
                    }
                }
            }

            if let Some(exit_status) = status {
                if stdout_reader.is_none() && stderr_reader.is_none() {
                    return Ok(Output {
                        status: exit_status,
                        stdout,
                        stderr,
                    });
                }
                let post_exit_at = *post_exit_deadline
                    .get_or_insert_with(|| process_deadline_after(PROCESS_POST_EXIT_DRAIN_TIMEOUT));
                let remaining = post_exit_at.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    let incomplete = CommandOutputCaptureIncomplete {
                        stdout_open: stdout_reader.is_some(),
                        stderr_open: stderr_reader.is_some(),
                        drain_timeout_ms: u64::try_from(
                            PROCESS_POST_EXIT_DRAIN_TIMEOUT.as_millis(),
                        )
                        .unwrap_or(u64::MAX),
                    };
                    // The leader has already been reaped by `try_wait`, so its
                    // PID/process-group identifier may be reusable and is no
                    // longer safe to signal. Closing our read endpoints is the
                    // only race-free action; inherited writers then observe a
                    // closed peer, while the caller receives a finite failure.
                    stdout_reader.take();
                    stderr_reader.take();
                    return Err(incomplete.into_io_error());
                }
                // A full fair-read quantum means there may already be more
                // buffered data. Re-enter the loop to check cancellation and
                // status, then continue draining without a readiness wait.
                // This avoids throttling high-FD macOS captures, whose safe
                // readiness fallback must otherwise sleep for one poll tick.
                if io_quantum_exhausted {
                    continue;
                }
                if let Err(error) = poll_process_io(
                    stdin_state.as_ref().map(|input| &input.writer),
                    stdout_reader.as_ref(),
                    stderr_reader.as_ref(),
                    bounded_output_poll_wait(remaining.min(PROCESS_POLL_INTERVAL), deadline),
                ) {
                    terminate_and_settle_output_command(
                        &mut child,
                        true,
                        CommandCleanupTrigger::ReadinessPoll,
                        &mut stdin_state,
                        &mut stdout_reader,
                        &mut stderr_reader,
                        &mut stdout,
                        &mut stderr,
                        limits,
                    )?;
                    return Err(error);
                }
            } else if !io_quantum_exhausted
                && let Err(error) = poll_process_io(
                    stdin_state.as_ref().map(|input| &input.writer),
                    stdout_reader.as_ref(),
                    stderr_reader.as_ref(),
                    bounded_output_poll_wait(PROCESS_POLL_INTERVAL, deadline),
                )
            {
                terminate_and_settle_output_command(
                    &mut child,
                    false,
                    CommandCleanupTrigger::ReadinessPoll,
                    &mut stdin_state,
                    &mut stdout_reader,
                    &mut stderr_reader,
                    &mut stdout,
                    &mut stderr,
                    limits,
                )?;
                return Err(error);
            }
        }
    }

    fn process_capture_socketpair(
        stream: &'static str,
    ) -> std::io::Result<(FileDescriptor, FileDescriptor)> {
        socketpair().map_err(|error| process_capture_setup_error(stream, error))
    }

    fn validate_command_input(stdin: Option<&[u8]>, limit: usize) -> std::io::Result<()> {
        if let Some(bytes) = stdin
            && bytes.len() > limit
        {
            return Err(CommandInputLimitExceeded {
                observed: bytes.len(),
                limit,
            }
            .into_io_error());
        }
        Ok(())
    }

    fn process_command_cancelled_error() -> std::io::Error {
        CommandCancelled.into_io_error()
    }

    fn check_output_command_abort(
        cancel: &AtomicBool,
        deadline: Option<OutputCommandDeadline>,
    ) -> std::io::Result<()> {
        if cancel.load(Ordering::SeqCst) {
            return Err(process_command_cancelled_error());
        }
        if let Some(deadline) = deadline
            && deadline.is_elapsed()
        {
            return Err(deadline.into_io_error());
        }
        Ok(())
    }

    fn bounded_output_poll_wait(
        requested: Duration,
        deadline: Option<OutputCommandDeadline>,
    ) -> Duration {
        deadline.map_or(requested, |deadline| requested.min(deadline.remaining()))
    }

    fn spawn_output_child(
        command: &mut std::process::Command,
        exec_busy_retry_delays: &[Duration],
        cancel: &AtomicBool,
        deadline: Option<OutputCommandDeadline>,
    ) -> std::io::Result<std::process::Child> {
        let mut retry_delays = exec_busy_retry_delays.iter().copied();
        loop {
            check_output_command_abort(cancel, deadline)?;
            match command.spawn() {
                Ok(child) => return Ok(child),
                Err(error) if is_exec_busy_error(&error) => {
                    let Some(delay) = retry_delays.next() else {
                        return Err(error);
                    };
                    wait_for_output_retry(delay, cancel, deadline)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(unix)]
    fn is_exec_busy_error(error: &std::io::Error) -> bool {
        const ETXTBSY: i32 = 26;
        error.raw_os_error() == Some(ETXTBSY)
    }

    #[cfg(not(unix))]
    fn is_exec_busy_error(_error: &std::io::Error) -> bool {
        false
    }

    fn wait_for_output_retry(
        delay: Duration,
        cancel: &AtomicBool,
        deadline: Option<OutputCommandDeadline>,
    ) -> std::io::Result<()> {
        let retry_deadline = process_deadline_after(delay);
        loop {
            check_output_command_abort(cancel, deadline)?;
            let remaining = retry_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            std::thread::sleep(bounded_output_poll_wait(
                remaining.min(PROCESS_POLL_INTERVAL),
                deadline,
            ));
        }
    }

    fn process_capture_setup_error(
        phase: &'static str,
        error: filedescriptor::Error,
    ) -> std::io::Error {
        std::io::Error::other(format!("process command I/O setup {phase} failed: {error}"))
    }

    fn append_captured_bytes(
        output: &mut Vec<u8>,
        bytes: &[u8],
        limit: usize,
        stream: CommandOutputStream,
    ) -> std::io::Result<()> {
        let observed = output.len().checked_add(bytes.len()).ok_or_else(|| {
            CommandOutputLimitExceeded::new(stream, usize::MAX, limit).into_io_error()
        })?;
        if observed > limit {
            return Err(CommandOutputLimitExceeded::new(stream, observed, limit).into_io_error());
        }
        reserve_capture_capacity(output, observed, limit)?;
        output.extend_from_slice(bytes);
        Ok(())
    }

    /// Grow geometrically within the caller's byte budget. Calling
    /// `try_reserve_exact(bytes.len())` for every 16 KiB read can otherwise
    /// force thousands of reallocations and copies for a legitimate large
    /// `get-text` response.
    fn reserve_capture_capacity(
        output: &mut Vec<u8>,
        observed: usize,
        limit: usize,
    ) -> std::io::Result<()> {
        if output.capacity() >= observed {
            return Ok(());
        }
        let geometric_target = output
            .capacity()
            .saturating_mul(2)
            .max(PROCESS_CAPTURE_INITIAL_RESERVE_BYTES)
            .max(observed)
            .min(limit);
        output
            .try_reserve_exact(geometric_target.saturating_sub(output.len()))
            .map_err(|_| std::io::Error::other("process command output capture allocation failed"))
    }

    /// Write a fair, bounded stdin quantum. Returning to the outer supervisor
    /// after `WouldBlock`/`Interrupted` keeps cancellation, deadline, status,
    /// and both output streams responsive even when the child reads slowly.
    fn write_command_input(input_state: &mut Option<CommandInputState>) -> std::io::Result<bool> {
        let Some(input) = input_state.as_mut() else {
            return Ok(false);
        };
        if input.written == input.bytes.len() {
            input_state.take();
            return Ok(false);
        }

        let mut written_this_turn = 0_usize;
        while written_this_turn < PROCESS_INPUT_WRITE_BYTES_PER_TURN {
            let remaining_quantum = PROCESS_INPUT_WRITE_BYTES_PER_TURN - written_this_turn;
            let remaining_input = input.bytes.len() - input.written;
            let write_len = remaining_quantum
                .min(remaining_input)
                .min(PROCESS_INPUT_WRITE_CHUNK_BYTES);
            match input
                .writer
                .write(&input.bytes[input.written..input.written + write_len])
            {
                Ok(0) => {
                    return Err(CommandInputWriteFailed {
                        error_kind: std::io::ErrorKind::WriteZero,
                        written: input.written,
                        total: input.bytes.len(),
                    }
                    .into_io_error());
                }
                Ok(written) => {
                    input.written += written;
                    written_this_turn += written;
                    if input.written == input.bytes.len() {
                        input_state.take();
                        return Ok(false);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(false);
                }
                Err(error) => {
                    return Err(CommandInputWriteFailed {
                        error_kind: error.kind(),
                        written: input.written,
                        total: input.bytes.len(),
                    }
                    .into_io_error());
                }
            }
        }
        Ok(true)
    }

    /// Drain a fair, bounded quantum so a continuously writing child cannot
    /// starve cancellation, status checks, or the sibling output stream.
    fn drain_process_capture(
        reader: &mut FileDescriptor,
        output: &mut Vec<u8>,
        limit: usize,
        stream: CommandOutputStream,
    ) -> std::io::Result<CaptureDrainState> {
        let mut drained = 0_usize;
        let mut chunk = [0_u8; PROCESS_CAPTURE_READ_CHUNK_BYTES];
        while drained < PROCESS_CAPTURE_READ_BYTES_PER_TURN {
            let remaining = PROCESS_CAPTURE_READ_BYTES_PER_TURN - drained;
            let chunk_len = remaining.min(chunk.len());
            match reader.read(&mut chunk[..chunk_len]) {
                Ok(0) => return Ok(CaptureDrainState::Eof),
                Ok(read) => {
                    append_captured_bytes(output, &chunk[..read], limit, stream)?;
                    drained += read;
                }
                // Return to the outer cancellation/status/deadline loop after
                // an interrupted syscall; an unbounded local retry could make
                // repeated signals defeat the settlement deadline.
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    return Ok(CaptureDrainState::Pending);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(CaptureDrainState::Pending);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(CaptureDrainState::QuantumExhausted)
    }

    fn drain_capture_reader(
        reader: &mut Option<FileDescriptor>,
        output: &mut Vec<u8>,
        limit: usize,
        stream: CommandOutputStream,
    ) -> std::io::Result<bool> {
        let Some(active_reader) = reader.as_mut() else {
            return Ok(false);
        };
        match drain_process_capture(active_reader, output, limit, stream)? {
            CaptureDrainState::Eof => {
                reader.take();
                Ok(false)
            }
            CaptureDrainState::Pending => Ok(false),
            CaptureDrainState::QuantumExhausted => Ok(true),
        }
    }

    fn poll_process_io(
        stdin: Option<&FileDescriptor>,
        stdout: Option<&FileDescriptor>,
        stderr: Option<&FileDescriptor>,
        wait: Duration,
    ) -> std::io::Result<()> {
        if wait.is_zero() {
            return Ok(());
        }
        let stdin_pollfd = stdin.map(|stdin| pollfd {
            fd: stdin.as_socket_descriptor(),
            events: POLLOUT,
            revents: 0,
        });
        let stdout_pollfd = stdout.map(|stdout| pollfd {
            fd: stdout.as_socket_descriptor(),
            events: POLLIN,
            revents: 0,
        });
        let stderr_pollfd = stderr.map(|stderr| pollfd {
            fd: stderr.as_socket_descriptor(),
            events: POLLIN,
            revents: 0,
        });
        match (stdin_pollfd, stdout_pollfd, stderr_pollfd) {
            (Some(stdin), Some(stdout), Some(stderr)) => {
                poll_capture_descriptors(&mut [stdin, stdout, stderr], wait)
            }
            (Some(stdin), Some(stdout), None) => {
                poll_capture_descriptors(&mut [stdin, stdout], wait)
            }
            (Some(stdin), None, Some(stderr)) => {
                poll_capture_descriptors(&mut [stdin, stderr], wait)
            }
            (None, Some(stdout), Some(stderr)) => {
                poll_capture_descriptors(&mut [stdout, stderr], wait)
            }
            (Some(descriptor), None, None)
            | (None, Some(descriptor), None)
            | (None, None, Some(descriptor)) => poll_capture_descriptors(&mut [descriptor], wait),
            (None, None, None) => {
                std::thread::sleep(wait);
                Ok(())
            }
        }
    }

    fn poll_capture_descriptors(readiness: &mut [pollfd], wait: Duration) -> std::io::Result<()> {
        match poll(readiness, Some(wait)) {
            Ok(_) => Ok(()),
            // filedescriptor implements poll with select(2) on macOS, where
            // high-numbered descriptors exceed FD_SETSIZE. Large sessions can
            // legitimately reach that state, so retain bounded nonblocking
            // reads and timer polling instead of failing the command.
            #[cfg(target_os = "macos")]
            Err(filedescriptor::Error::FdValueOutsideFdSetSize(_)) => {
                std::thread::sleep(wait);
                Ok(())
            }
            Err(error) if filedescriptor_error_is_interrupted(&error) => Ok(()),
            Err(error) => Err(std::io::Error::other(error)),
        }
    }

    fn filedescriptor_error_is_interrupted(error: &filedescriptor::Error) -> bool {
        let mut source: &(dyn std::error::Error + 'static) = error;
        loop {
            if source
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::Interrupted)
            {
                return true;
            }
            let Some(next) = source.source() else {
                return false;
            };
            source = next;
        }
    }

    fn process_deadline_after(duration: Duration) -> Instant {
        let now = Instant::now();
        let mut bounded = duration;
        loop {
            if let Some(deadline) = now.checked_add(bounded) {
                return deadline;
            }

            // `Instant` has a platform-specific finite range. An overflowing
            // operator timeout must not become an immediate timeout: clamp it
            // toward a representable far future by halving only on the
            // exceptional overflow path. Zero is always representable, so the
            // loop is finite even on an unusually narrow `Instant` platform.
            bounded = bounded.checked_div(2).unwrap_or(Duration::ZERO);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn terminate_and_settle_output_command(
        child: &mut std::process::Child,
        leader_already_reaped: bool,
        trigger: CommandCleanupTrigger,
        stdin_state: &mut Option<CommandInputState>,
        stdout_reader: &mut Option<FileDescriptor>,
        stderr_reader: &mut Option<FileDescriptor>,
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
        limits: OutputCaptureLimits,
    ) -> std::io::Result<()> {
        // Closing the owned writer before termination guarantees that every
        // supervisor failure path stops retaining secret-bearing input and
        // gives a still-running child an immediate EOF opportunity.
        stdin_state.take();
        // One shared deadline covers the leader probe, external group-signal
        // helper, direct owned-child kill, reap, and descriptor drain. The
        // helper receives this exact deadline, so its internal 100 ms phases
        // cannot add another 200 ms after the advertised settlement budget.
        let settlement_deadline = process_deadline_after(PROCESS_TERMINATION_SETTLE_TIMEOUT);
        let (reaped, signal_helper_settled, process_tree_signalled) =
            terminate_output_child_if_running(child, leader_already_reaped, settlement_deadline);
        let cleanup = settle_output_command(
            child,
            reaped,
            signal_helper_settled,
            process_tree_signalled,
            stdout_reader,
            stderr_reader,
            stdout,
            stderr,
            limits,
            settlement_deadline,
        );
        ensure_output_cleanup_complete(trigger, cleanup)
    }

    /// Best-effort bounded drain and reap after cancellation or capture
    /// failure. It never calls `Child::wait` and never waits for inherited
    /// descriptors past the fixed settlement deadline.
    #[allow(clippy::too_many_arguments)]
    fn settle_output_command(
        child: &mut std::process::Child,
        mut reaped: bool,
        signal_helper_settled: bool,
        process_tree_signalled: bool,
        stdout_reader: &mut Option<FileDescriptor>,
        stderr_reader: &mut Option<FileDescriptor>,
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
        limits: OutputCaptureLimits,
        deadline: Instant,
    ) -> OutputCleanupState {
        loop {
            let stdout_quantum_exhausted = match drain_capture_reader(
                stdout_reader,
                stdout,
                limits.stdout,
                CommandOutputStream::Stdout,
            ) {
                Ok(quantum_exhausted) => quantum_exhausted,
                Err(_) => {
                    stdout_reader.take();
                    false
                }
            };
            let stderr_quantum_exhausted = match drain_capture_reader(
                stderr_reader,
                stderr,
                limits.stderr,
                CommandOutputStream::Stderr,
            ) {
                Ok(quantum_exhausted) => quantum_exhausted,
                Err(_) => {
                    stderr_reader.take();
                    false
                }
            };

            if !reaped {
                match child.try_wait() {
                    Ok(Some(_)) => reaped = true,
                    Ok(None) => {}
                    Err(error)
                        if output_child_probe_error_action(&error)
                            == OutputChildProbeErrorAction::RetryUntilDeadline => {}
                    Err(_) => {
                        return output_cleanup_state(
                            reaped,
                            signal_helper_settled,
                            process_tree_signalled,
                            stdout_reader.as_ref(),
                            stderr_reader.as_ref(),
                        );
                    }
                }
            }
            if reaped && stdout_reader.is_none() && stderr_reader.is_none() {
                return output_cleanup_state(
                    reaped,
                    signal_helper_settled,
                    process_tree_signalled,
                    stdout_reader.as_ref(),
                    stderr_reader.as_ref(),
                );
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return output_cleanup_state(
                    reaped,
                    signal_helper_settled,
                    process_tree_signalled,
                    stdout_reader.as_ref(),
                    stderr_reader.as_ref(),
                );
            }
            if stdout_quantum_exhausted || stderr_quantum_exhausted {
                continue;
            }
            let wait = remaining.min(PROCESS_POLL_INTERVAL);
            if poll_process_io(None, stdout_reader.as_ref(), stderr_reader.as_ref(), wait).is_err()
            {
                std::thread::sleep(wait);
            }
        }
    }

    fn output_cleanup_state(
        leader_reaped: bool,
        signal_helper_settled: bool,
        process_tree_signalled: bool,
        stdout_reader: Option<&FileDescriptor>,
        stderr_reader: Option<&FileDescriptor>,
    ) -> OutputCleanupState {
        OutputCleanupState {
            leader_reaped,
            signal_helper_settled,
            process_tree_signalled,
            stdout_open: stdout_reader.is_some(),
            stderr_open: stderr_reader.is_some(),
        }
    }

    fn ensure_output_cleanup_complete(
        trigger: CommandCleanupTrigger,
        cleanup: OutputCleanupState,
    ) -> std::io::Result<()> {
        if cleanup.leader_reaped
            && cleanup.signal_helper_settled
            && cleanup.process_tree_signalled
            && !cleanup.stdout_open
            && !cleanup.stderr_open
        {
            return Ok(());
        }

        Err(CommandProcessCleanupIncomplete {
            trigger,
            leader_reaped: cleanup.leader_reaped,
            signal_helper_settled: cleanup.signal_helper_settled,
            process_tree_signalled: cleanup.process_tree_signalled,
            stdout_open: cleanup.stdout_open,
            stderr_open: cleanup.stderr_open,
            settle_timeout_ms: u64::try_from(PROCESS_TERMINATION_SETTLE_TIMEOUT.as_millis())
                .unwrap_or(u64::MAX),
        }
        .into_io_error())
    }

    fn output_cleanup_trigger_for_capture_error(error: &std::io::Error) -> CommandCleanupTrigger {
        CommandOutputLimitExceeded::from_io_error(error)
            .map_or(CommandCleanupTrigger::CaptureRead, |exceeded| {
                CommandCleanupTrigger::CaptureLimit(exceeded.stream())
            })
    }

    /// Probe immediately before any numeric PID/process-group signal. A
    /// successful reap makes that identity reusable, so it must never be
    /// signalled. Interrupted probes retry only until the shared settlement
    /// deadline. Every other probe error fails closed without signalling
    /// because the leader state is unknown.
    fn terminate_output_child_if_running(
        child: &mut std::process::Child,
        leader_already_reaped: bool,
        probe_deadline: Instant,
    ) -> (bool, bool, bool) {
        if leader_already_reaped {
            // Once the leader has been reaped, its numeric PID/process-group
            // identity may already have been reused. We therefore skip the
            // external group signal. Report the leader and (nonexistent)
            // helper as settled, but never claim that the process tree was
            // signalled when no such signal was safely issued.
            return (true, true, false);
        }

        loop {
            match child.try_wait() {
                // The child exited before we could signal its process group.
                // Its PID is now reusable, so treat the leader as reaped but
                // do not fabricate process-tree termination proof.
                Ok(Some(_)) => return (true, true, false),
                Ok(None) => {
                    let (signal_helper_settled, process_tree_signalled) =
                        terminate_child_process(child, probe_deadline);
                    return (false, signal_helper_settled, process_tree_signalled);
                }
                Err(error)
                    if output_child_probe_error_action(&error)
                        == OutputChildProbeErrorAction::RetryUntilDeadline =>
                {
                    let remaining = probe_deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return (false, true, false);
                    }
                    std::thread::sleep(remaining.min(PROCESS_POLL_INTERVAL));
                }
                Err(_) => return (false, true, false),
            }
        }
    }

    fn output_child_probe_error_action(error: &std::io::Error) -> OutputChildProbeErrorAction {
        if error.kind() == std::io::ErrorKind::Interrupted {
            OutputChildProbeErrorAction::RetryUntilDeadline
        } else {
            OutputChildProbeErrorAction::StopWithUncertainIdentity
        }
    }

    /// This helper is called only immediately after `try_wait` returned
    /// `Ok(None)`. On Unix, no other owner can reap this `&mut Child` between
    /// that probe and these signals, so its PID cannot be reused; on Windows,
    /// `Child::kill` retains the process handle identity. The helper makes no
    /// safety claim for arbitrary numeric process identifiers.
    fn terminate_child_process(child: &mut std::process::Child, deadline: Instant) -> (bool, bool) {
        let group_result = send_signal_to_process_group_until(child.id(), "KILL", deadline);
        let signal_helper_settled = group_result.as_ref().err().is_none_or(|error| {
            CommandSignalHelperCleanupIncomplete::from_io_error(error).is_none()
        });
        let process_tree_signalled = group_result.is_ok_and(|status| status.success());
        let _ = child.kill();
        (signal_helper_settled, process_tree_signalled)
    }

    #[cfg(test)]
    mod output_capture_unit_tests {
        use super::*;

        #[test]
        fn command_capture_defaults_are_finite_and_setters_are_exact() {
            assert!(DEFAULT_COMMAND_STDOUT_LIMIT_BYTES > 0);
            assert!(DEFAULT_COMMAND_STDOUT_LIMIT_BYTES < usize::MAX);
            assert!(DEFAULT_COMMAND_STDERR_LIMIT_BYTES > 0);
            assert!(DEFAULT_COMMAND_STDERR_LIMIT_BYTES < usize::MAX);
            assert!(DEFAULT_COMMAND_STDIN_LIMIT_BYTES > 0);
            assert!(DEFAULT_COMMAND_STDIN_LIMIT_BYTES < usize::MAX);

            let mut command = Command::new("not-executed");
            assert!(command.stdin.is_none());
            assert!(command.stdin_configuration_error.is_none());
            assert_eq!(command.stdin_limit, DEFAULT_COMMAND_STDIN_LIMIT_BYTES);
            assert_eq!(command.stdout_limit, DEFAULT_COMMAND_STDOUT_LIMIT_BYTES);
            assert_eq!(command.stderr_limit, DEFAULT_COMMAND_STDERR_LIMIT_BYTES);
            command.stdin_limit(5).stdout_limit(17).stderr_limit(9);
            assert_eq!(command.stdin_limit, 5);
            assert_eq!(command.stdout_limit, 17);
            assert_eq!(command.stderr_limit, 9);
        }

        #[test]
        fn command_cancellation_is_idempotent_and_typed_content_free() {
            let cancellation = CommandCancellation::new();
            assert!(!cancellation.is_cancelled());
            cancellation.cancel();
            cancellation.cancel();
            assert!(cancellation.is_cancelled());

            let error = process_command_cancelled_error();
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
            assert!(CommandCancelled::from_io_error(&error).is_some());
            assert_eq!(error.to_string(), "process command cancelled");
        }

        #[test]
        fn command_deadline_error_is_typed_and_content_free() {
            let deadline = OutputCommandDeadline::new(Duration::from_millis(37));
            let error = deadline.into_io_error();
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            let timed_out = CommandTimedOut::from_io_error(&error)
                .expect("deadline error must retain stable typed detail");
            assert_eq!(timed_out.timeout_ms(), 37);
            assert_eq!(error.to_string(), "process command timed out after 37 ms");
        }

        #[test]
        fn overflowing_deadline_clamps_to_a_future_instant() {
            let before = Instant::now();
            let deadline = process_deadline_after(Duration::MAX);
            assert!(deadline > before);
            assert!(!OutputCommandDeadline::new(Duration::MAX).is_elapsed());
        }

        #[test]
        fn child_probe_errors_retry_only_for_interruption() {
            let interrupted = std::io::Error::from(std::io::ErrorKind::Interrupted);
            assert_eq!(
                output_child_probe_error_action(&interrupted),
                OutputChildProbeErrorAction::RetryUntilDeadline
            );

            for kind in [
                std::io::ErrorKind::Other,
                std::io::ErrorKind::PermissionDenied,
                std::io::ErrorKind::InvalidInput,
            ] {
                assert_eq!(
                    output_child_probe_error_action(&std::io::Error::from(kind)),
                    OutputChildProbeErrorAction::StopWithUncertainIdentity
                );
            }
        }

        #[cfg(unix)]
        #[test]
        fn exec_busy_classifier_is_narrow() {
            assert!(is_exec_busy_error(&std::io::Error::from_raw_os_error(26)));
            assert!(!is_exec_busy_error(&std::io::Error::from_raw_os_error(22)));
            assert!(!is_exec_busy_error(&std::io::Error::from(
                std::io::ErrorKind::Interrupted
            )));
        }

        #[test]
        fn exec_busy_retry_wait_checks_cancel_and_deadline_before_sleeping() {
            let cancelled = AtomicBool::new(true);
            let error = wait_for_output_retry(Duration::MAX, &cancelled, None)
                .expect_err("pre-cancelled retry wait must not sleep");
            assert!(CommandCancelled::from_io_error(&error).is_some());

            let active = AtomicBool::new(false);
            let deadline = OutputCommandDeadline::new(Duration::ZERO);
            let error = wait_for_output_retry(Duration::MAX, &active, Some(deadline))
                .expect_err("elapsed command deadline must stop retry wait");
            assert!(CommandTimedOut::from_io_error(&error).is_some());
        }

        #[test]
        fn already_reaped_leader_never_claims_process_tree_was_signalled() {
            let source = include_str!("runtime_async.rs");
            let start = source
                .find("    fn terminate_output_child_if_running(")
                .expect("termination implementation marker");
            let end = source[start..]
                .find("    fn output_child_probe_error_action(")
                .map(|offset| start + offset)
                .expect("termination implementation end marker");
            let implementation = &source[start..end];
            let reaped_branch_start = implementation
                .find("if leader_already_reaped")
                .expect("already-reaped branch marker");
            let reaped_branch_end = implementation[reaped_branch_start..]
                .find("\n        loop {")
                .map(|offset| reaped_branch_start + offset)
                .expect("already-reaped branch end marker");
            let reaped_branch = &implementation[reaped_branch_start..reaped_branch_end];
            assert!(reaped_branch.contains("return (true, true, false);"));
            assert!(!reaped_branch.contains("return (true, true, true);"));
            assert!(!implementation.contains("return (true, true, true);"));
        }

        #[test]
        fn signal_helper_uncertainty_is_typed_and_content_free() {
            let error = CommandSignalHelperCleanupIncomplete {
                phase: CommandSignalHelperFailurePhase::CompletionProbe,
                probe_error_kind: Some(std::io::ErrorKind::Other),
            }
            .into_io_error();
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            let incomplete = CommandSignalHelperCleanupIncomplete::from_io_error(&error)
                .expect("signal helper uncertainty must retain stable typed detail");
            assert_eq!(
                incomplete.phase(),
                CommandSignalHelperFailurePhase::CompletionProbe
            );
            assert_eq!(
                incomplete.probe_error_kind(),
                Some(std::io::ErrorKind::Other)
            );
            assert!(!error.to_string().contains("raw-command-or-output-canary"));
        }

        #[test]
        fn cleanup_incomplete_error_is_typed_structural_and_content_free() {
            let cleanup = OutputCleanupState {
                leader_reaped: false,
                signal_helper_settled: true,
                process_tree_signalled: false,
                stdout_open: true,
                stderr_open: false,
            };
            let error = ensure_output_cleanup_complete(
                CommandCleanupTrigger::CaptureLimit(CommandOutputStream::Stdout),
                cleanup,
            )
            .expect_err("unreaped leader must not report successful cleanup");
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            let incomplete = CommandProcessCleanupIncomplete::from_io_error(&error)
                .expect("cleanup failure must retain stable typed detail");
            assert_eq!(
                incomplete.trigger(),
                CommandCleanupTrigger::CaptureLimit(CommandOutputStream::Stdout)
            );
            assert!(!incomplete.leader_reaped());
            assert!(incomplete.signal_helper_settled());
            assert!(!incomplete.process_tree_signalled());
            assert!(incomplete.stdout_open());
            assert!(!incomplete.stderr_open());
            assert_eq!(incomplete.settle_timeout_ms(), 250);
            assert!(!error.to_string().contains("raw-child-output-canary"));

            ensure_output_cleanup_complete(
                CommandCleanupTrigger::Cancelled,
                OutputCleanupState {
                    leader_reaped: true,
                    signal_helper_settled: true,
                    process_tree_signalled: true,
                    stdout_open: false,
                    stderr_open: false,
                },
            )
            .expect("fully settled process must preserve its initiating result");
        }

        #[test]
        fn output_spec_snapshots_limits_directory_and_bounded_retries() {
            let mut command = Command::new("not-executed");
            command
                .current_dir("relative-not-executed")
                .stdin_bytes(b"owned-input".as_slice())
                .stdin_limit(19)
                .stdout_limit(23)
                .stderr_limit(11)
                .exec_busy_retry_delays(&[Duration::from_millis(3)]);
            let spec = command.output_spec();
            assert_eq!(
                spec.current_dir.as_deref(),
                Some(std::path::Path::new("relative-not-executed"))
            );
            assert_eq!(spec.stdin.as_deref(), Some(b"owned-input".as_slice()));
            assert!(spec.stdin_configuration_error.is_none());
            assert_eq!(spec.stdin_limit, 19);
            assert_eq!(spec.limits.stdout, 23);
            assert_eq!(spec.limits.stderr, 11);
            assert_eq!(spec.exec_busy_retry_delays, [Duration::from_millis(3)]);
        }

        #[test]
        fn stdin_limit_accepts_exact_boundary_and_rejects_one_byte_over() {
            validate_command_input(Some(&[1, 2, 3]), 3)
                .expect("exact stdin boundary must be accepted");
            let error = validate_command_input(Some(&[1, 2, 3]), 2)
                .expect_err("first byte beyond stdin limit must fail before spawn");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            let exceeded = CommandInputLimitExceeded::from_io_error(&error)
                .expect("stdin limit failure must retain stable typed detail");
            assert_eq!(exceeded.observed(), 3);
            assert_eq!(exceeded.limit(), 2);

            let mut command = Command::new("not-executed");
            command.stdin_limit(2).stdin_bytes(vec![1, 2, 3]);
            assert!(
                command.stdin.is_none(),
                "oversized input must not be retained"
            );
            let retained_error = command
                .stdin_configuration_error
                .expect("oversized setter input must retain only typed detail");
            assert_eq!(retained_error.observed(), 3);
            assert_eq!(retained_error.limit(), 2);
        }

        #[test]
        fn nonblocking_stdin_writer_delivers_owned_bytes_and_closes_at_eof() {
            let (mut writer, mut reader) = socketpair().expect("stdin test socketpair");
            writer
                .set_non_blocking(true)
                .expect("nonblocking stdin test writer");
            let mut state = Some(CommandInputState {
                writer,
                bytes: Arc::from(b"secret-owned-stdin".as_slice()),
                written: 0,
            });
            assert!(!write_command_input(&mut state).expect("bounded stdin write"));
            assert!(state.is_none(), "completed input must close its writer");

            let mut received = Vec::new();
            reader
                .read_to_end(&mut received)
                .expect("closed writer gives deterministic EOF");
            assert_eq!(received, b"secret-owned-stdin");
        }

        #[test]
        fn stdin_write_error_is_typed_and_does_not_echo_payload() {
            let error = CommandInputWriteFailed {
                error_kind: std::io::ErrorKind::BrokenPipe,
                written: 7,
                total: 19,
            }
            .into_io_error();
            let failed = CommandInputWriteFailed::from_io_error(&error)
                .expect("stdin write failure must retain stable typed detail");
            assert_eq!(failed.error_kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(failed.written(), 7);
            assert_eq!(failed.total(), 19);
            assert!(!error.to_string().contains("raw-stdin-payload-canary"));
        }

        #[cfg(unix)]
        #[test]
        fn command_stdin_bytes_roundtrip_through_bounded_supervisor() {
            let mut command = Command::new("sh");
            command
                .args(["-c", "cat"])
                .stdin_bytes(b"bounded stdin roundtrip".as_slice())
                .stdin_limit(64)
                .stdout_limit(64)
                .stderr_limit(64);
            let output = command
                .output_blocking(Duration::from_secs(2))
                .expect("bounded stdin roundtrip command");
            assert!(output.status.success());
            assert_eq!(output.stdout, b"bounded stdin roundtrip");
            assert!(output.stderr.is_empty());
        }

        #[cfg(unix)]
        #[test]
        fn blocked_stdin_write_observes_command_deadline() {
            let mut command = Command::new("sh");
            command
                .args(["-c", "sleep 5"])
                .stdin_bytes(vec![b'x'; 1024 * 1024])
                .stdin_limit(1024 * 1024);
            let started = Instant::now();
            let error = command
                .output_blocking(Duration::from_millis(25))
                .expect_err("non-reading child must not defeat command deadline");
            assert!(CommandTimedOut::from_io_error(&error).is_some());
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        #[cfg(unix)]
        #[test]
        fn blocked_stdin_write_observes_cooperative_cancellation() {
            let cancellation = CommandCancellation::new();
            let cancel_from_thread = cancellation.clone();
            let trigger = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(25));
                cancel_from_thread.cancel();
            });
            let mut command = Command::new("sh");
            command
                .args(["-c", "sleep 5"])
                .stdin_bytes(vec![b'x'; 1024 * 1024])
                .stdin_limit(1024 * 1024);
            let started = Instant::now();
            let error = command
                .output_blocking_with_cancellation(Duration::from_secs(5), &cancellation)
                .expect_err("non-reading child must observe cooperative cancellation");
            trigger.join().expect("cancellation trigger must finish");
            assert!(CommandCancelled::from_io_error(&error).is_some());
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        #[test]
        fn valid_capture_text_reuses_its_owned_allocation() {
            let bytes = b"valid UTF-8 capture".to_vec();
            let original_pointer = bytes.as_ptr();
            let text = decode_captured_bytes_lossy(bytes);
            assert_eq!(text, "valid UTF-8 capture");
            assert_eq!(text.as_ptr(), original_pointer);
        }

        #[test]
        fn invalid_capture_text_retains_lossy_replacement_semantics() {
            let text = decode_captured_bytes_lossy(vec![b'a', 0xff, b'b']);
            assert_eq!(text, "a\u{fffd}b");
        }

        #[test]
        fn capture_reservation_grows_geometrically_and_reuses_capacity() {
            let mut output = Vec::new();
            append_captured_bytes(&mut output, b"a", 1024, CommandOutputStream::Stdout)
                .expect("first bounded reservation");
            let first_capacity = output.capacity();
            let first_pointer = output.as_ptr();
            assert!(first_capacity >= 101);

            append_captured_bytes(&mut output, &[b'b'; 100], 1024, CommandOutputStream::Stdout)
                .expect("existing geometric reservation should accept the next chunk");
            assert_eq!(output.as_ptr(), first_pointer);
            assert_eq!(output.len(), 101);
        }

        #[test]
        fn capture_accepts_exact_limit_and_rejects_first_excess_chunk() {
            let mut output = vec![1_u8, 2];
            append_captured_bytes(&mut output, &[3, 4], 4, CommandOutputStream::Stdout)
                .expect("exact capture limit must be accepted");
            assert_eq!(output, vec![1, 2, 3, 4]);

            let error = append_captured_bytes(
                &mut output,
                b"secret-output-must-not-enter-error",
                4,
                CommandOutputStream::Stdout,
            )
            .expect_err("first byte beyond the limit must fail closed");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            let exceeded = CommandOutputLimitExceeded::from_io_error(&error)
                .expect("capture overflow must retain its stable typed class");
            assert_eq!(exceeded.stream(), CommandOutputStream::Stdout);
            assert_eq!(exceeded.limit(), 4);
            assert!(exceeded.observed() > exceeded.limit());
            assert!(!error.to_string().contains("secret-output"));
            assert_eq!(output, vec![1, 2, 3, 4]);
        }

        #[test]
        fn zero_capture_limit_allows_empty_output_only() {
            let mut output = Vec::new();
            append_captured_bytes(&mut output, &[], 0, CommandOutputStream::Stderr)
                .expect("empty output is valid under a zero-byte cap");
            let error = append_captured_bytes(&mut output, b"x", 0, CommandOutputStream::Stderr)
                .expect_err("non-empty output must exceed a zero-byte cap");
            let exceeded = CommandOutputLimitExceeded::from_io_error(&error)
                .expect("stderr overflow must retain typed detail");
            assert_eq!(exceeded.stream(), CommandOutputStream::Stderr);
            assert_eq!(exceeded.observed(), 1);
            assert_eq!(exceeded.limit(), 0);
        }

        #[test]
        fn incomplete_capture_error_is_typed_content_free_and_timed_out() {
            let error = CommandOutputCaptureIncomplete {
                stdout_open: true,
                stderr_open: false,
                drain_timeout_ms: 100,
            }
            .into_io_error();
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            let incomplete = CommandOutputCaptureIncomplete::from_io_error(&error)
                .expect("incomplete capture must retain stable typed detail");
            assert!(incomplete.stdout_open());
            assert!(!incomplete.stderr_open());
            assert_eq!(incomplete.drain_timeout_ms(), 100);
            assert_eq!(
                error.to_string(),
                "process command output capture incomplete after 100 ms (stdout_open=true, stderr_open=false)"
            );
        }

        #[test]
        fn production_capture_has_no_unbounded_reader_or_reap_primitive() {
            let source = include_str!("runtime_async.rs");
            let start = source
                .find("    fn run_output_command(")
                .expect("capture implementation marker");
            let end = source[start..]
                .find("    #[cfg(test)]\n    mod output_capture_unit_tests")
                .map(|offset| start + offset)
                .expect("capture unit-test marker");
            let implementation = &source[start..end];
            assert!(
                !implementation.contains("terminate_child_process(&mut child)"),
                "capture paths must probe through terminate_output_child_if_running"
            );
            for forbidden in [
                "read_to_end",
                "std::thread::Builder",
                "std::thread::spawn",
                ".join()",
                "child.wait()",
            ] {
                assert!(
                    !implementation.contains(forbidden),
                    "production capture must not contain {forbidden}"
                );
            }
        }

        #[test]
        fn production_signal_helpers_have_no_unbounded_wait_primitive() {
            let source = include_str!("runtime_async.rs");
            let start = source
                .find("    trait ProcessControl")
                .expect("process-control implementation marker");
            let end = source[start..]
                .find("    impl KillOnDropGuard")
                .map(|offset| start + offset)
                .expect("process-control implementation end marker");
            let implementation = &source[start..end];
            for forbidden in [".status()", ".wait()", "wait_with_output", "read_to_end"] {
                assert!(
                    !implementation.contains(forbidden),
                    "production signal helper must not contain {forbidden}"
                );
            }
            assert!(implementation.contains("reap_signal_helper_until"));
            assert!(implementation.contains("PROCESS_SIGNAL_HELPER_TIMEOUT"));
            assert!(implementation.contains("PROCESS_SIGNAL_HELPER_REAP_TIMEOUT"));
        }
    }

    #[cfg(all(test, windows))]
    mod windows_process_control_tests {
        use super::*;

        #[test]
        fn windows_taskkill_force_maps_term_and_kill() {
            assert!(!windows_taskkill_force("TERM").unwrap());
            assert!(!windows_taskkill_force("SIGTERM").unwrap());
            assert!(!windows_taskkill_force("sigterm").unwrap());
            assert!(windows_taskkill_force("KILL").unwrap());
            assert!(windows_taskkill_force("SIGKILL").unwrap());
            assert!(windows_taskkill_force("sigkill").unwrap());
        }

        #[test]
        fn windows_taskkill_force_rejects_unknown_or_shell_like_signals() {
            assert!(windows_taskkill_force("HUP").is_err());
            assert!(windows_taskkill_force("TERM;cmd").is_err());
        }

        #[test]
        fn windows_signal_target_must_be_positive_process_id() {
            assert!(validate_windows_signal_target(1).is_ok());
            assert!(validate_windows_signal_target(0).is_err());
            assert!(validate_windows_signal_target(-1).is_err());
            assert!(validate_windows_signal_target(i64::from(u32::MAX) + 1).is_err());
        }
    }

    #[cfg(test)]
    mod watcher_done_guard_tests {
        // br-ft-xffjo: pin the RAII contract that
        // `WatcherDoneGuard::Drop` sets the inner `AtomicBool` to
        // true exactly once, regardless of construction site exit
        // path. This is the structural guarantee that closes the
        // `output_with_cx` watcher-leak window — if Drop fails to
        // signal, the watcher tight-loops until cx cancel.
        use super::WatcherDoneGuard;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        #[test]
        fn watcher_done_guard_sets_flag_on_drop() {
            let flag = Arc::new(AtomicBool::new(false));
            {
                let _guard = WatcherDoneGuard::new(Arc::clone(&flag));
                assert!(
                    !flag.load(Ordering::SeqCst),
                    "guard must NOT eagerly set the flag — only on Drop",
                );
            }
            assert!(
                flag.load(Ordering::SeqCst),
                "guard's Drop must set the flag — without this the \
                 cx-watcher in output_with_cx would leak past the \
                 function body on `?` early-exit (br-ft-xffjo)",
            );
        }

        #[test]
        fn watcher_done_guard_drop_is_idempotent_with_explicit_set() {
            // The happy path in output_with_cx sets `watcher_done`
            // explicitly before draining the watcher_handle, then
            // the guard's Drop fires when the function returns.
            // This sequence must be safe (no panic, no double-effect).
            let flag = Arc::new(AtomicBool::new(false));
            {
                let _guard = WatcherDoneGuard::new(Arc::clone(&flag));
                // Caller pre-sets the flag (the normal-path drain).
                flag.store(true, Ordering::SeqCst);
                // Guard's Drop now fires — must be a no-op since
                // store(true, true) is idempotent.
            }
            assert!(flag.load(Ordering::SeqCst));
        }

        #[test]
        fn watcher_done_guard_signals_on_panic_unwind() {
            // The most important RAII property: a panic inside the
            // guard's scope must still trigger Drop. This pins the
            // "function body panic" exit path — without RAII, a
            // panic between guard construction and the explicit
            // `watcher_done.store(true)` would leak the watcher
            // forever (the panic skips the explicit store and the
            // watcher_handle.await drain).
            let flag = Arc::new(AtomicBool::new(false));
            let flag_inner = Arc::clone(&flag);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = WatcherDoneGuard::new(flag_inner);
                panic!("simulated panic inside watcher-guarded scope");
            }));
            assert!(result.is_err(), "panic must propagate up");
            assert!(
                flag.load(Ordering::SeqCst),
                "Drop must fire on panic unwind — that's the whole \
                 point of RAII over manual cleanup",
            );
        }
    }
}

/// Async I/O traits for the active runtime.
///
/// Re-exports the extension traits needed for TCP stream I/O.
/// For Unix-specific I/O (BufReader, lines, etc.) see the `unix` module.
pub mod io {
    pub use asupersync::io::{
        AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
    };

    /// Read some bytes from an async reader into `buf`, returning how many
    /// bytes were read. Polyfill for tokio's `AsyncReadExt::read` which
    /// asupersync does not yet provide.
    pub async fn read<R: asupersync::io::AsyncRead + Unpin>(
        reader: &mut R,
        buf: &mut [u8],
    ) -> std::io::Result<usize> {
        std::future::poll_fn(|cx| {
            let mut read_buf = asupersync::io::ReadBuf::new(buf);
            match std::pin::Pin::new(&mut *reader).poll_read(cx, &mut read_buf) {
                std::task::Poll::Ready(Ok(())) => {
                    std::task::Poll::Ready(Ok(read_buf.filled().len()))
                }
                std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        })
        .await
    }
}

/// Async networking primitives for the active runtime.
///
/// For Unix sockets, see the `unix` module.
pub mod net {
    pub use asupersync::net::{TcpListener, TcpStream};
}

/// Canonical distributed TLS transport surface.
///
/// Keeping these constructors behind the project-owned async module prevents
/// first-party callers from coupling directly to asupersync's module layout.
#[cfg(feature = "distributed")]
pub mod tls {
    pub use asupersync::tls::{TlsAcceptor, TlsConnector};
}

/// Signal handling primitives for graceful shutdown.
///
/// Wraps `asupersync::signal` for the asupersync runtime.
pub mod signal {
    /// Completes when a Ctrl+C (SIGINT) signal is received.
    ///
    /// # Errors
    ///
    /// Returns an error if the signal handler could not be registered.
    pub async fn ctrl_c() -> std::io::Result<()> {
        asupersync::signal::ctrl_c().await
    }

    /// Unix-specific signal handling.
    #[cfg(unix)]
    pub mod unix {
        /// Signal kinds for Unix signal handling.
        pub struct SignalKind(asupersync::signal::SignalKind);

        impl SignalKind {
            /// Returns the `SIGINT` signal kind.
            pub fn interrupt() -> Self {
                Self(asupersync::signal::SignalKind::interrupt())
            }

            /// Returns the `SIGTERM` signal kind.
            pub fn terminate() -> Self {
                Self(asupersync::signal::SignalKind::terminate())
            }

            /// Returns the `SIGHUP` signal kind.
            pub fn hangup() -> Self {
                Self(asupersync::signal::SignalKind::hangup())
            }
        }

        /// A stream of signals of a specific kind.
        pub struct Signal {
            inner: asupersync::signal::Signal,
        }

        impl Signal {
            /// Receives the next signal notification.
            ///
            /// Returns `None` if the signal stream is terminated.
            pub async fn recv(&mut self) -> Option<()> {
                self.inner.recv().await
            }
        }

        /// Creates a new listener for the given signal kind.
        ///
        /// # Errors
        ///
        /// Returns an error if the signal handler could not be registered.
        pub fn signal(kind: SignalKind) -> std::io::Result<Signal> {
            asupersync::signal::signal(kind.0).map(|inner| Signal { inner })
        }
    }
}

/// Unified runtime trait for async lifecycle management.
pub trait CompatRuntime {
    /// Runs a future to completion.
    fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future;

    /// Spawns a detached task.
    fn spawn_detached<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static;
}

/// Runtime wrapper for asupersync.
pub struct Runtime {
    inner: asupersync::runtime::Runtime,
    shutdown_token: RuntimeShutdownToken,
}

const RUNTIME_SHUTDOWN_LEASE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

impl Drop for Runtime {
    fn drop(&mut self) {
        saturating_increment_counter(&RUNTIME_SHUTDOWN_REQUESTED_TOTAL);
        if self
            .shutdown_token
            .request_shutdown_and_wait(RUNTIME_SHUTDOWN_LEASE_DRAIN_TIMEOUT)
        {
            saturating_increment_counter(&RUNTIME_SHUTDOWN_DRAINED_TOTAL);
        } else {
            saturating_increment_counter(&RUNTIME_SHUTDOWN_DRAIN_TIMEOUT_TOTAL);
            tracing::warn!(
                timeout_ms = RUNTIME_SHUTDOWN_LEASE_DRAIN_TIMEOUT.as_millis(),
                "runtime shutdown cleanup leases did not settle before the finite drain deadline"
            );
        }
    }
}

/// Asupersync implementation of the project runtime lifecycle trait.
impl CompatRuntime for Runtime {
    fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        // Install the RuntimeHandle into thread-local storage so that
        // task::spawn can find it without requiring callers to pass it
        // explicitly. This mirrors tokio's ambient runtime context.
        let handle = self.inner.handle();
        ASUPERSYNC_HANDLE.with(|cell| cell.replace(Some(handle)));
        ASUPERSYNC_SHUTDOWN_TOKEN.with(|cell| cell.replace(Some(self.shutdown_token.clone())));
        let result = self.inner.block_on(future);
        // Negative-evidence ledger (ft-2worp): intentionally do NOT clear the
        // handle at block_on return. Clearing it here previously produced
        // process-aborting `thread local panicked on drop` failures when the
        // pinned asupersync Runtime later shut down during thread teardown.
        // The retained handle owns an Arc and is replaced by the next block_on;
        // test harnesses clear it only after catching Runtime::drop. Scoped
        // restoration remains appropriate for spawned-future *polls*, where
        // the runtime itself is not being torn down.
        result
    }

    fn spawn_detached<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = self.inner.handle();
        let task_cx = crate::cx::Cx::current();
        let task_cap_mask = task_cx.as_ref().map(crate::cx::effective_cap_mask);
        // Wrap in HandleContextFuture so that nested task::spawn() calls
        // inside the detached future can find the runtime handle in
        // thread-local storage. Preserve the ambient Cx and effective
        // capability mask as well: RuntimeHandle::spawn otherwise supplies a
        // root scheduler Cx and a detached child could escape cancellation or
        // regain denied authority.
        let wrapped = task::HandleContextFuture {
            shutdown_token: Some(self.shutdown_token.clone()),
            task_cx,
            task_cap_mask,
            future: Box::pin(future),
        };
        std::mem::drop(handle.spawn(wrapped));
    }
}

/// Runtime builder wrapper for the active backend.
pub struct RuntimeBuilder {
    inner: asupersync::runtime::RuntimeBuilder,
}

/// Default blocking-pool ceiling for wrapper-built runtimes (ft-7p1bx).
///
/// asupersync's `BlockingPoolConfig` defaults to `max_threads: 0`, which
/// means NO blocking pool — and `asupersync::runtime::spawn_blocking`
/// under an ambient `Cx` without a pool runs the closure INLINE on the
/// executor thread. On a current-thread runtime that freezes the entire
/// async world for the duration of the blocking work: timers don't fire,
/// cancel watchers can't poll, `timeout(..)` can't elapse. Every wrapper
/// preset therefore configures a real pool. Threads spawn on demand
/// (min 0) and are reaped after the pool's idle timeout, so idle cost is
/// zero; the ceiling is deterministic and host-independent.
const DEFAULT_MAX_BLOCKING_THREADS: usize = 16;

impl RuntimeBuilder {
    #[must_use]
    pub fn current_thread() -> Self {
        Self {
            inner: asupersync::runtime::RuntimeBuilder::current_thread()
                .blocking_threads(0, DEFAULT_MAX_BLOCKING_THREADS),
        }
    }

    #[must_use]
    pub fn multi_thread() -> Self {
        Self {
            inner: asupersync::runtime::RuntimeBuilder::new()
                .blocking_threads(0, DEFAULT_MAX_BLOCKING_THREADS),
        }
    }

    #[must_use]
    pub fn worker_threads(self, n: usize) -> Self {
        Self {
            inner: self.inner.worker_threads(n),
        }
    }

    /// Bound the blocking pool (tokio-parity knob).
    ///
    /// The wrapper presets already configure an on-demand pool of up to
    /// [`DEFAULT_MAX_BLOCKING_THREADS`]; callers with heavier blocking
    /// fan-out (bulk SQLite scans, process bridges) can raise the ceiling.
    /// A `max` of 0 would drop the pool entirely and revert
    /// `spawn_blocking` to inline-on-executor execution, so it is clamped
    /// to 1.
    #[must_use]
    pub fn max_blocking_threads(self, max: usize) -> Self {
        Self {
            inner: self.inner.blocking_threads(0, max.max(1)),
        }
    }

    /// No-op: asupersync handles I/O and timers automatically.
    #[must_use]
    pub fn enable_all(self) -> Self {
        self
    }

    /// No-op: thread naming is not exposed in asupersync.
    #[must_use]
    pub fn thread_name(self, _name: &str) -> Self {
        self
    }

    pub fn build(self) -> Result<Runtime, String> {
        self.inner
            .build()
            .map(|inner| Runtime {
                inner,
                shutdown_token: RuntimeShutdownToken::new(),
            })
            .map_err(|err| err.to_string())
    }
}

/// Sleep for the specified duration using asupersync.
pub async fn sleep(duration: Duration) {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    let _ = sleep_with_cx(&cx, duration).await;
}

/// Sleep for the requested duration while respecting the provided `Cx`.
///
/// This is the Cx-first sleep seam used by the ft-xbnl0.2.2 migration. Call
/// sites that have a `Cx` in hand should prefer this over the ambient
/// [`sleep`] which falls back to `Cx::current()` thread-local lookup.
///
/// # Cancellation semantics (ft-xbnl0.2.4 tick 331)
///
/// Mirrors [`timeout_with_cx`]: this function observes the cx **budget
/// deadline** (via [`asupersync::time::budget_sleep`], which caps the
/// effective sleep duration by remaining budget), but does **not**
/// directly check `cx.is_cancel_requested()`. A pre-cancelled cx with
/// an infinite budget will still sleep for the full requested
/// `duration` before returning `Ok`.
///
/// Callers who need a sleep that short-circuits on cancel MUST add an
/// explicit `cx.checkpoint()?` (or equivalent `if cx.is_cancel_requested()`
/// bail) **before** invoking `sleep_with_cx`, and MUST re-check cancel
/// at loop iteration boundaries when using this inside a polling loop.
/// Every cx-first accept/poll loop in this crate that uses `sleep_with_cx`
/// already follows this pattern (e.g. watchdog.rs, backpressure polling).
pub async fn sleep_with_cx(cx: &crate::cx::Cx, duration: Duration) -> Result<(), String> {
    asupersync::time::budget_sleep(cx, duration, cx_timer_now(cx))
        .await
        .map_err(|err| err.to_string())
}

/// Maximum number of concurrently admitted interruptible timer registrations.
///
/// One entry consists of one asupersync wheel entry plus one inline
/// cancellation waiter. The hard cap keeps aggregate timer memory finite while
/// leaving ample headroom above the synthetic 1,000-follower campaign tier.
pub const INTERRUPTIBLE_TIMER_CAPACITY: usize = 65_536;

/// Largest single delay admitted by the interruptible timer service.
///
/// This is deliberately below asupersync 0.3.5's seven-day wheel clamp. A
/// larger request is refused instead of silently waking early and extending a
/// lease, cursor deadline, or caller timeout incorrectly.
pub const INTERRUPTIBLE_TIMER_MAX_DELAY: Duration = Duration::from_hours(24);

/// Content-free aggregate counters for the bounded interruptible timer service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptibleTimerMetrics {
    /// Hard concurrent-registration limit.
    pub capacity: usize,
    /// Currently admitted registrations.
    pub active: usize,
    /// Successful admissions since process start.
    pub admissions: u64,
    /// Calls refused because the hard limit was already active.
    pub saturations: u64,
    /// Calls refused because their delay exceeded the exact project bound.
    pub duration_refusals: u64,
    /// Calls refused because the supplied `Cx` was not the active timer context.
    pub context_refusals: u64,
    /// Admitted timers interrupted by direct context cancellation.
    pub cancellations: u64,
    /// Admitted timers ended by a capability deadline.
    pub deadline_expirations: u64,
    /// Admitted timers ended by poll-quota or cost-budget exhaustion.
    pub budget_exhaustions: u64,
    /// Admitted timers ended by an unattributed context failure.
    pub context_failures: u64,
    /// Timer deadlines that woke and reached the completion branch.
    pub wake_completions: u64,
    /// Re-polls that observed neither timer readiness nor cancellation.
    pub stale_wakeups: u64,
    /// Admitted registrations removed while handling `Shutdown` cancellation.
    pub shutdown_cleanups: u64,
    /// Largest observed delay from effective deadline to completion wake.
    pub max_wake_latency_ns: u64,
}

struct InterruptibleTimerService {
    capacity: usize,
    active: std::sync::atomic::AtomicUsize,
    admissions: std::sync::atomic::AtomicU64,
    saturations: std::sync::atomic::AtomicU64,
    duration_refusals: std::sync::atomic::AtomicU64,
    context_refusals: std::sync::atomic::AtomicU64,
    cancellations: std::sync::atomic::AtomicU64,
    deadline_expirations: std::sync::atomic::AtomicU64,
    budget_exhaustions: std::sync::atomic::AtomicU64,
    context_failures: std::sync::atomic::AtomicU64,
    wake_completions: std::sync::atomic::AtomicU64,
    stale_wakeups: std::sync::atomic::AtomicU64,
    shutdown_cleanups: std::sync::atomic::AtomicU64,
    max_wake_latency_ns: std::sync::atomic::AtomicU64,
}

impl InterruptibleTimerService {
    const fn new(capacity: usize) -> Self {
        Self {
            capacity,
            active: std::sync::atomic::AtomicUsize::new(0),
            admissions: std::sync::atomic::AtomicU64::new(0),
            saturations: std::sync::atomic::AtomicU64::new(0),
            duration_refusals: std::sync::atomic::AtomicU64::new(0),
            context_refusals: std::sync::atomic::AtomicU64::new(0),
            cancellations: std::sync::atomic::AtomicU64::new(0),
            deadline_expirations: std::sync::atomic::AtomicU64::new(0),
            budget_exhaustions: std::sync::atomic::AtomicU64::new(0),
            context_failures: std::sync::atomic::AtomicU64::new(0),
            wake_completions: std::sync::atomic::AtomicU64::new(0),
            stale_wakeups: std::sync::atomic::AtomicU64::new(0),
            shutdown_cleanups: std::sync::atomic::AtomicU64::new(0),
            max_wake_latency_ns: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn try_admit(&self) -> Result<InterruptibleTimerAdmission<'_>, SleepWithCxError> {
        let mut active = self.active.load(std::sync::atomic::Ordering::Acquire);
        loop {
            if active >= self.capacity {
                saturating_increment_counter(&self.saturations);
                return Err(SleepWithCxError {
                    kind: SleepWithCxErrorKind::TimerCapacityExhausted,
                });
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => active = observed,
            }
        }
        saturating_increment_counter(&self.admissions);
        Ok(InterruptibleTimerAdmission { service: self })
    }

    fn classify_and_record_context_termination(
        &self,
        cx: &crate::cx::Cx,
        fallback: SleepWithCxErrorKind,
    ) -> SleepWithCxError {
        let error = SleepWithCxError::from_cx(cx, fallback);
        match error.kind() {
            SleepWithCxErrorKind::ContextCancelled => {
                saturating_increment_counter(&self.cancellations);
                if cx
                    .root_cancel_cause()
                    .is_some_and(|reason| reason.kind == crate::outcome::CancelKind::Shutdown)
                {
                    saturating_increment_counter(&self.shutdown_cleanups);
                }
            }
            SleepWithCxErrorKind::DeadlineExceeded => {
                saturating_increment_counter(&self.deadline_expirations);
            }
            SleepWithCxErrorKind::PollQuotaExhausted
            | SleepWithCxErrorKind::CostBudgetExhausted => {
                saturating_increment_counter(&self.budget_exhaustions);
            }
            SleepWithCxErrorKind::ContextFailure
            | SleepWithCxErrorKind::TimerCapacityExhausted
            | SleepWithCxErrorKind::TimerDurationExceeded
            | SleepWithCxErrorKind::TimerContextUnavailable => {
                saturating_increment_counter(&self.context_failures);
            }
        }
        error
    }

    fn record_duration_refusal(&self) {
        saturating_increment_counter(&self.duration_refusals);
    }

    fn record_wake_completion(&self, latency_ns: u64) {
        saturating_increment_counter(&self.wake_completions);
        self.max_wake_latency_ns
            .fetch_max(latency_ns, std::sync::atomic::Ordering::Relaxed);
    }

    fn snapshot(&self) -> InterruptibleTimerMetrics {
        InterruptibleTimerMetrics {
            capacity: self.capacity,
            active: self.active.load(std::sync::atomic::Ordering::Acquire),
            admissions: self.admissions.load(std::sync::atomic::Ordering::Relaxed),
            saturations: self.saturations.load(std::sync::atomic::Ordering::Relaxed),
            duration_refusals: self
                .duration_refusals
                .load(std::sync::atomic::Ordering::Relaxed),
            context_refusals: self
                .context_refusals
                .load(std::sync::atomic::Ordering::Relaxed),
            cancellations: self
                .cancellations
                .load(std::sync::atomic::Ordering::Relaxed),
            deadline_expirations: self
                .deadline_expirations
                .load(std::sync::atomic::Ordering::Relaxed),
            budget_exhaustions: self
                .budget_exhaustions
                .load(std::sync::atomic::Ordering::Relaxed),
            context_failures: self
                .context_failures
                .load(std::sync::atomic::Ordering::Relaxed),
            wake_completions: self
                .wake_completions
                .load(std::sync::atomic::Ordering::Relaxed),
            stale_wakeups: self
                .stale_wakeups
                .load(std::sync::atomic::Ordering::Relaxed),
            shutdown_cleanups: self
                .shutdown_cleanups
                .load(std::sync::atomic::Ordering::Relaxed),
            max_wake_latency_ns: self
                .max_wake_latency_ns
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

struct InterruptibleTimerAdmission<'a> {
    service: &'a InterruptibleTimerService,
}

impl Drop for InterruptibleTimerAdmission<'_> {
    fn drop(&mut self) {
        let previous = self
            .service
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        debug_assert!(previous > 0, "interruptible timer admission underflow");
    }
}

static INTERRUPTIBLE_TIMER_SERVICE: InterruptibleTimerService =
    InterruptibleTimerService::new(INTERRUPTIBLE_TIMER_CAPACITY);

/// Read the bounded timer service's redacted, eventually consistent aggregate
/// counters.
#[must_use]
pub fn interruptible_timer_metrics() -> InterruptibleTimerMetrics {
    INTERRUPTIBLE_TIMER_SERVICE.snapshot()
}

/// Finite failure class for [`sleep_with_cx_interruptible`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepWithCxErrorKind {
    /// The caller explicitly cancelled or shut down the capability context.
    ContextCancelled,
    /// The caller's capability deadline or timeout elapsed first.
    DeadlineExceeded,
    /// The caller exhausted its cooperative poll quota.
    PollQuotaExhausted,
    /// The caller exhausted its cost budget.
    CostBudgetExhausted,
    /// The bounded timer service refused a new registration at capacity.
    TimerCapacityExhausted,
    /// The requested delay exceeded the exact project timer bound.
    TimerDurationExceeded,
    /// The supplied context was not the active timer-capable task context.
    TimerContextUnavailable,
    /// The context failed without an attributable structural cause.
    ContextFailure,
}

/// Content-free error returned by [`sleep_with_cx_interruptible`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepWithCxError {
    kind: SleepWithCxErrorKind,
}

impl SleepWithCxError {
    fn from_cx(cx: &crate::cx::Cx, fallback: SleepWithCxErrorKind) -> Self {
        use crate::outcome::CancelKind;

        let kind = match cx.root_cancel_cause().map(|reason| reason.kind) {
            Some(CancelKind::Deadline | CancelKind::Timeout) => {
                SleepWithCxErrorKind::DeadlineExceeded
            }
            Some(CancelKind::PollQuota) => SleepWithCxErrorKind::PollQuotaExhausted,
            Some(CancelKind::CostBudget) => SleepWithCxErrorKind::CostBudgetExhausted,
            Some(
                CancelKind::User
                | CancelKind::FailFast
                | CancelKind::RaceLost
                | CancelKind::ParentCancelled
                | CancelKind::ResourceUnavailable
                | CancelKind::Shutdown
                | CancelKind::LinkedExit,
            ) => SleepWithCxErrorKind::ContextCancelled,
            None => fallback,
        };
        Self { kind }
    }

    /// Return the finite structural failure class.
    #[must_use]
    pub const fn kind(self) -> SleepWithCxErrorKind {
        self.kind
    }

    /// Return true only for caller-context cancellation, not budget exhaustion.
    #[must_use]
    pub const fn is_cancelled(self) -> bool {
        matches!(self.kind, SleepWithCxErrorKind::ContextCancelled)
    }
}

impl std::fmt::Display for SleepWithCxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self.kind {
            SleepWithCxErrorKind::ContextCancelled => "sleep cancelled by capability context",
            SleepWithCxErrorKind::DeadlineExceeded => "sleep capability deadline exceeded",
            SleepWithCxErrorKind::PollQuotaExhausted => "sleep capability poll quota exhausted",
            SleepWithCxErrorKind::CostBudgetExhausted => "sleep capability cost budget exhausted",
            SleepWithCxErrorKind::TimerCapacityExhausted => {
                "interruptible timer capacity exhausted"
            }
            SleepWithCxErrorKind::TimerDurationExceeded => {
                "interruptible timer duration exceeds the supported bound"
            }
            SleepWithCxErrorKind::TimerContextUnavailable => {
                "interruptible timer requires the active timer-capable context"
            }
            SleepWithCxErrorKind::ContextFailure => "sleep capability context failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SleepWithCxError {}

/// Sleep on one asupersync timer-wheel registration and wake promptly when the
/// supplied caller context is cancelled.
///
/// Unlike [`sleep_with_cx`], this helper races the budget-aware timer against a
/// cancel-aware waiter that registers the current task waker directly with the
/// explicit `Cx`. Cancellation therefore re-polls this future without a polling
/// timer, watcher task, or blocking-pool job. The supplied context must be the
/// active timer-capable task context so the timer, budget, cancellation waker,
/// and wake-latency clock remain in one domain. The timer is dropped and
/// deregistered when cancellation wins.
///
/// A `Cx` cancellation waker is task-scoped. Concurrent timers must therefore
/// use their own task-owned contexts rather than driving this helper from
/// multiple scheduler tasks that share one cloned `Cx`.
///
/// # Errors
///
/// Returns a finite, content-free [`SleepWithCxError`] for direct cancellation,
/// capability-budget exhaustion, registration-capacity exhaustion, a delay
/// above [`INTERRUPTIBLE_TIMER_MAX_DELAY`], or a supplied `Cx` that is not the
/// active timer-capable task context. Cancellation is checked before validating
/// or arming the timer and again after the timer wins, closing the
/// ready-vs-cancel race without exposing a post-cancellation success.
pub async fn sleep_with_cx_interruptible(
    cx: &crate::cx::Cx,
    duration: Duration,
) -> Result<(), SleepWithCxError> {
    sleep_with_cx_interruptible_using(cx, duration, &INTERRUPTIBLE_TIMER_SERVICE).await
}

async fn sleep_with_cx_interruptible_using(
    cx: &crate::cx::Cx,
    duration: Duration,
    service: &InterruptibleTimerService,
) -> Result<(), SleepWithCxError> {
    if cx.checkpoint().is_err() {
        return Err(SleepWithCxError::from_cx(
            cx,
            SleepWithCxErrorKind::ContextFailure,
        ));
    }
    if duration.is_zero() {
        return cx
            .checkpoint()
            .map_err(|_error| SleepWithCxError::from_cx(cx, SleepWithCxErrorKind::ContextFailure));
    }
    if duration > INTERRUPTIBLE_TIMER_MAX_DELAY {
        service.record_duration_refusal();
        return Err(SleepWithCxError {
            kind: SleepWithCxErrorKind::TimerDurationExceeded,
        });
    }

    let active_context_matches = crate::cx::Cx::current().is_some_and(|active| {
        active.region_id() == cx.region_id()
            && active.task_id() == cx.task_id()
            && active.timer_driver().is_some()
            && cx.timer_driver().is_some()
    });
    if !active_context_matches {
        saturating_increment_counter(&service.context_refusals);
        return Err(SleepWithCxError {
            kind: SleepWithCxErrorKind::TimerContextUnavailable,
        });
    }

    let _admission = service.try_admit()?;

    use futures::future::{Either, select};

    let timer_started_at = cx_timer_now(cx);
    let requested_deadline = timer_started_at + duration;
    let effective_deadline = cx.budget().deadline.map_or(requested_deadline, |deadline| {
        deadline.min(requested_deadline)
    });
    let timer = std::pin::pin!(asupersync::time::budget_sleep(
        cx,
        duration,
        timer_started_at,
    ));
    // An uninitialized OnceCell is a zero-payload cancellation signal here:
    // `wait` registers directly with the explicit Cx and can only resolve via
    // cancellation because this private cell is never initialized.
    let cancellation_signal = asupersync::sync::OnceCell::<()>::new();
    let mut cancellation_wait = std::pin::pin!(cancellation_signal.wait(cx));
    let mut cancellation_poll_count = 0_u64;
    let cancellation = std::pin::pin!(std::future::poll_fn(|task_cx| {
        let poll = cancellation_wait.as_mut().poll(task_cx);
        if poll.is_pending() {
            if cancellation_poll_count > 0 {
                saturating_increment_counter(&service.stale_wakeups);
            }
            cancellation_poll_count = cancellation_poll_count.saturating_add(1);
        }
        poll
    }));

    match select(timer, cancellation).await {
        Either::Left((Ok(()), _)) => {
            service.record_wake_completion(cx_timer_now(cx).duration_since(effective_deadline));
            cx.checkpoint().map_err(|_error| {
                service.classify_and_record_context_termination(
                    cx,
                    SleepWithCxErrorKind::ContextFailure,
                )
            })
        }
        Either::Left((Err(_elapsed), _)) => {
            // budget_sleep's sole error is an elapsed capability deadline.
            // Checkpoint once to latch the structural cancel cause when this
            // sleep was the first observer of deadline exhaustion.
            let _ = cx.checkpoint();
            Err(service.classify_and_record_context_termination(
                cx,
                SleepWithCxErrorKind::DeadlineExceeded,
            ))
        }
        Either::Right((_wait_result, _)) => Err(service
            .classify_and_record_context_termination(cx, SleepWithCxErrorKind::ContextFailure)),
    }
}

/// Terminal class for [`write_all_nonblocking_with_cx`].
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonblockingWriteErrorKind {
    /// The owning capability context was cancelled before completion.
    ContextCancelled,
    /// The writer returned `Ok(0)` while bytes remained.
    WriteZero,
    /// A non-retryable descriptor write failed.
    Write,
    /// The required flush boundary failed.
    Flush,
    /// The runtime could not register or re-arm writable readiness.
    Readiness,
    /// Output did not cross its flush boundary within the caller's exact bound.
    OutputTimeout,
}

/// Exact progress receipt for a completed nonblocking write and flush.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonblockingWriteReceipt {
    bytes_written: usize,
    blocked_duration_ns: u64,
}

#[cfg(unix)]
impl NonblockingWriteReceipt {
    /// Number of bytes acknowledged by the writer before flush succeeded.
    #[must_use]
    pub const fn bytes_written(self) -> usize {
        self.bytes_written
    }

    /// Time spent after the first `WouldBlock`, in the caller's timer domain.
    #[must_use]
    pub const fn blocked_duration_ns(self) -> u64 {
        self.blocked_duration_ns
    }
}

/// Exact byte progress and finite failure class for a nonblocking write.
#[cfg(unix)]
#[derive(Debug)]
pub struct NonblockingWriteError {
    kind: NonblockingWriteErrorKind,
    bytes_written: usize,
    blocked_duration_ns: u64,
    cancellation_latency_upper_bound_ns: u64,
    source: Option<std::io::Error>,
}

#[cfg(unix)]
impl NonblockingWriteError {
    fn new(
        kind: NonblockingWriteErrorKind,
        bytes_written: usize,
        blocked_duration_ns: u64,
        source: Option<std::io::Error>,
    ) -> Self {
        Self {
            kind,
            bytes_written,
            blocked_duration_ns,
            cancellation_latency_upper_bound_ns: 0,
            source,
        }
    }

    fn cancelled(
        bytes_written: usize,
        blocked_duration_ns: u64,
        cancellation_latency_upper_bound_ns: u64,
    ) -> Self {
        Self {
            kind: NonblockingWriteErrorKind::ContextCancelled,
            bytes_written,
            blocked_duration_ns,
            cancellation_latency_upper_bound_ns,
            source: None,
        }
    }

    /// Return the finite structural failure class.
    #[must_use]
    pub const fn kind(&self) -> NonblockingWriteErrorKind {
        self.kind
    }

    /// Return the exact number of bytes accepted before failure.
    #[must_use]
    pub const fn bytes_written(&self) -> usize {
        self.bytes_written
    }

    /// Return time spent after the first `WouldBlock`, in the caller's clock.
    #[must_use]
    pub const fn blocked_duration_ns(&self) -> u64 {
        self.blocked_duration_ns
    }

    /// Conservative time from the last pending poll to cancellation
    /// settlement. The cancellation request occurs within this interval, so
    /// the value is an upper bound rather than a fabricated exact timestamp.
    #[must_use]
    pub const fn cancellation_latency_upper_bound_ns(&self) -> u64 {
        self.cancellation_latency_upper_bound_ns
    }

    /// Return true only when the owning capability context stopped the write.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, NonblockingWriteErrorKind::ContextCancelled)
    }

    /// Consume the receipt and return its underlying descriptor error, if any.
    pub fn into_source(self) -> Option<std::io::Error> {
        self.source
    }
}

#[cfg(unix)]
impl std::fmt::Display for NonblockingWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self.kind {
            NonblockingWriteErrorKind::ContextCancelled => {
                "nonblocking write cancelled by capability context"
            }
            NonblockingWriteErrorKind::WriteZero => "nonblocking writer made zero progress",
            NonblockingWriteErrorKind::Write => "nonblocking descriptor write failed",
            NonblockingWriteErrorKind::Flush => "nonblocking descriptor flush failed",
            NonblockingWriteErrorKind::Readiness => {
                "nonblocking writable-readiness registration failed"
            }
            NonblockingWriteErrorKind::OutputTimeout => {
                "nonblocking write exceeded its output-completion bound"
            }
        };
        write!(
            formatter,
            "{message} after {} acknowledged bytes",
            self.bytes_written
        )
    }
}

#[cfg(unix)]
impl std::error::Error for NonblockingWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(unix)]
fn nonblocking_write_blocked_duration_ns(
    cx: &crate::cx::Cx,
    blocked_started_at: Option<asupersync::Time>,
) -> u64 {
    blocked_started_at.map_or(0, |started_at| cx_timer_now(cx).duration_since(started_at))
}

/// Write one complete byte slice and cross its flush boundary using a
/// nonblocking Unix descriptor registered with the owning asupersync reactor.
///
/// The function records exact accepted-byte progress, retries `Interrupted`,
/// and parks on one writable-readiness registration after `WouldBlock`. The
/// registration is re-armed before retrying and dropped on every terminal
/// path. A second write attempt immediately after first registration closes
/// the register-vs-ready race for the required single-owner writer.
/// `maximum_output_duration` bounds the complete write-plus-flush interval;
/// its timer is allocated and armed only after the first `WouldBlock`, while
/// active fragmented progress checks the same deadline cooperatively.
///
/// # Errors
///
/// Returns [`NonblockingWriteError`] with exact byte progress when the caller
/// is cancelled, the output-completion bound expires, the writer reports zero
/// progress, a write/flush fails, or the runtime cannot register writable
/// readiness. Callers must treat every error with non-zero progress as partial
/// or ambiguous delivery.
#[cfg(unix)]
pub async fn write_all_nonblocking_with_cx<W>(
    cx: &crate::cx::Cx,
    writer: &mut W,
    bytes: &[u8],
    maximum_output_duration: Duration,
) -> Result<NonblockingWriteReceipt, NonblockingWriteError>
where
    W: std::io::Write + asupersync::runtime::Source,
{
    use asupersync::runtime::Interest;
    use futures::future::{Either, select};

    enum WritePhase {
        Bytes,
        Flush,
    }

    let mut phase = WritePhase::Bytes;
    let output_started_at = cx_timer_now(cx);
    let last_pending_at_ns = std::sync::atomic::AtomicU64::new(output_started_at.as_nanos());
    let bytes_written = std::sync::atomic::AtomicUsize::new(0);
    let blocked_started_at_ns = std::sync::atomic::AtomicU64::new(0);
    let blocked_started = std::sync::atomic::AtomicBool::new(false);
    let mut readiness: Option<asupersync::runtime::IoRegistration> = None;
    let mut consecutive_interruptions = 0_u16;
    let mut blocked_timeout: Option<futures::future::BoxFuture<'_, ()>> = None;

    let progress = || bytes_written.load(std::sync::atomic::Ordering::Relaxed);
    let blocked_duration = || {
        if blocked_started.load(std::sync::atomic::Ordering::Acquire) {
            nonblocking_write_blocked_duration_ns(
                cx,
                Some(asupersync::Time::from_nanos(
                    blocked_started_at_ns.load(std::sync::atomic::Ordering::Relaxed),
                )),
            )
        } else {
            0
        }
    };

    if cx.checkpoint().is_err() {
        return Err(NonblockingWriteError::cancelled(
            progress(),
            blocked_duration(),
            0,
        ));
    }

    let output = std::pin::pin!(std::future::poll_fn(|task_cx| {
        let polled_at = cx_timer_now(cx);
        if cx.checkpoint().is_err() {
            return std::task::Poll::Ready(Err(NonblockingWriteError::cancelled(
                progress(),
                blocked_duration(),
                polled_at.duration_since(asupersync::Time::from_nanos(
                    last_pending_at_ns.load(std::sync::atomic::Ordering::Relaxed),
                )),
            )));
        }
        let output_elapsed_ns = polled_at.duration_since(output_started_at);
        if u128::from(output_elapsed_ns) >= maximum_output_duration.as_nanos() {
            return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                NonblockingWriteErrorKind::OutputTimeout,
                progress(),
                blocked_duration(),
                None,
            )));
        }
        if let Some(timeout) = blocked_timeout.as_mut()
            && timeout.as_mut().poll(task_cx).is_ready()
        {
            // `budget_sleep` also settles when an earlier capability deadline
            // terminates the owning Cx. Re-check after polling it so caller
            // cancellation/deadline remains control flow instead of being
            // misreported as the longer output-completion timeout (which could
            // otherwise authorize a terminal follow-up record).
            let settled_at = cx_timer_now(cx);
            if cx.checkpoint().is_err() {
                return std::task::Poll::Ready(Err(NonblockingWriteError::cancelled(
                    progress(),
                    blocked_duration(),
                    settled_at.duration_since(asupersync::Time::from_nanos(
                        last_pending_at_ns.load(std::sync::atomic::Ordering::Relaxed),
                    )),
                )));
            }
            return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                NonblockingWriteErrorKind::OutputTimeout,
                progress(),
                blocked_duration(),
                None,
            )));
        }

        if let Some(registration) = readiness.as_mut() {
            match registration.rearm(Interest::WRITABLE, task_cx.waker()) {
                Ok(true) => {}
                Ok(false) => readiness = None,
                Err(source) => {
                    return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                        NonblockingWriteErrorKind::Readiness,
                        progress(),
                        blocked_duration(),
                        Some(source),
                    )));
                }
            }
        }

        // Bound one cooperative poll even for pathological writers that keep
        // returning Interrupted or tiny successful fragments.
        for _ in 0..64 {
            let had_readiness = readiness.is_some();
            let acknowledged = progress();
            let result = match phase {
                WritePhase::Bytes if acknowledged < bytes.len() => {
                    writer.write(&bytes[acknowledged..]).map(Some)
                }
                WritePhase::Bytes => {
                    phase = WritePhase::Flush;
                    continue;
                }
                WritePhase::Flush => writer.flush().map(|()| None),
            };

            match result {
                Ok(Some(0)) => {
                    return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                        NonblockingWriteErrorKind::WriteZero,
                        progress(),
                        blocked_duration(),
                        None,
                    )));
                }
                Ok(Some(written)) => {
                    let remaining = bytes.len().saturating_sub(progress());
                    if written > remaining {
                        return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                            NonblockingWriteErrorKind::Write,
                            progress(),
                            blocked_duration(),
                            Some(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "writer reported progress beyond the supplied buffer",
                            )),
                        )));
                    }
                    bytes_written.fetch_add(written, std::sync::atomic::Ordering::Relaxed);
                    consecutive_interruptions = 0;
                    readiness = None;
                }
                Ok(None) => {
                    readiness = None;
                    return std::task::Poll::Ready(Ok(NonblockingWriteReceipt {
                        bytes_written: progress(),
                        blocked_duration_ns: blocked_duration(),
                    }));
                }
                Err(source) if source.kind() == std::io::ErrorKind::Interrupted => {
                    consecutive_interruptions = consecutive_interruptions.saturating_add(1);
                    if consecutive_interruptions == 1_024 {
                        let kind = match phase {
                            WritePhase::Bytes => NonblockingWriteErrorKind::Write,
                            WritePhase::Flush => NonblockingWriteErrorKind::Flush,
                        };
                        return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                            kind,
                            progress(),
                            blocked_duration(),
                            Some(source),
                        )));
                    }
                }
                Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                    consecutive_interruptions = 0;
                    if !blocked_started.load(std::sync::atomic::Ordering::Relaxed) {
                        use futures::FutureExt as _;

                        let active_context_matches =
                            crate::cx::Cx::current().is_some_and(|active| {
                                active.region_id() == cx.region_id()
                                    && active.task_id() == cx.task_id()
                                    && active.timer_driver().is_some()
                                    && cx.timer_driver().is_some()
                            });
                        if !active_context_matches {
                            return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                                NonblockingWriteErrorKind::Readiness,
                                progress(),
                                blocked_duration(),
                                Some(std::io::Error::new(
                                    std::io::ErrorKind::Unsupported,
                                    "nonblocking output requires the active timer-capable context",
                                )),
                            )));
                        }

                        let started_at = cx_timer_now(cx);
                        blocked_started_at_ns
                            .store(started_at.as_nanos(), std::sync::atomic::Ordering::Relaxed);
                        blocked_started.store(true, std::sync::atomic::Ordering::Release);
                        blocked_timeout = Some(
                            async move {
                                let elapsed = Duration::from_nanos(
                                    started_at.duration_since(output_started_at),
                                );
                                let _ = asupersync::time::budget_sleep(
                                    cx,
                                    maximum_output_duration.saturating_sub(elapsed),
                                    started_at,
                                )
                                .await;
                            }
                            .boxed(),
                        );
                        // The lazily constructed timer must be polled once to
                        // register before descriptor readiness can park us.
                        task_cx.waker().wake_by_ref();
                    }
                    if !had_readiness {
                        let mut registration = match cx.register_io(writer, Interest::WRITABLE) {
                            Ok(registration) => registration,
                            Err(source) => {
                                return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                                    NonblockingWriteErrorKind::Readiness,
                                    progress(),
                                    blocked_duration(),
                                    Some(source),
                                )));
                            }
                        };
                        match registration.rearm(Interest::WRITABLE, task_cx.waker()) {
                            Ok(true) => readiness = Some(registration),
                            Ok(false) => continue,
                            Err(source) => {
                                return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                                    NonblockingWriteErrorKind::Readiness,
                                    progress(),
                                    blocked_duration(),
                                    Some(source),
                                )));
                            }
                        }
                        // Retry once while registered. If readiness raced with
                        // registration, the single owner now observes progress
                        // instead of depending on a possibly consumed wake.
                        continue;
                    }
                    last_pending_at_ns.store(
                        cx_timer_now(cx).as_nanos(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    return std::task::Poll::Pending;
                }
                Err(source) => {
                    let kind = match phase {
                        WritePhase::Bytes => NonblockingWriteErrorKind::Write,
                        WritePhase::Flush => NonblockingWriteErrorKind::Flush,
                    };
                    return std::task::Poll::Ready(Err(NonblockingWriteError::new(
                        kind,
                        progress(),
                        blocked_duration(),
                        Some(source),
                    )));
                }
            }
        }

        last_pending_at_ns.store(
            cx_timer_now(cx).as_nanos(),
            std::sync::atomic::Ordering::Relaxed,
        );
        task_cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }));
    // The private cell is never initialized. Its wait future registers the
    // current task waker directly with the explicit Cx and resolves only when
    // cancellation, a deadline, or a capability budget terminates the Cx.
    // `select` polls `output` first, so a completed flush wins a same-poll
    // cancellation race and remains a truthful downstream acknowledgement.
    let cancellation_signal = asupersync::sync::OnceCell::<()>::new();
    let cancellation = std::pin::pin!(cancellation_signal.wait(cx));
    if let Either::Left((result, _cancellation)) = select(output, cancellation).await {
        return result;
    }
    // The losing output future's pinned borrow ends with the `if let`
    // temporary above, before these captured progress cells are inspected.
    let cancellation_latency_upper_bound_ns = cx_timer_now(cx).duration_since(
        asupersync::Time::from_nanos(last_pending_at_ns.load(std::sync::atomic::Ordering::Relaxed)),
    );
    Err(NonblockingWriteError::cancelled(
        progress(),
        blocked_duration(),
        cancellation_latency_upper_bound_ns,
    ))
}

/// Pause without inheriting the ambient capability budget.
///
/// Reserved for trusted terminal-settlement loops that must retain ownership
/// after their caller context has failed. The starting timestamp still comes
/// from the ambient timer driver when one exists, preserving deterministic
/// runtime clock domains without making cancellation turn the pause into a
/// zero-duration spin.
async fn sleep_unbudgeted(duration: Duration) {
    let now = crate::cx::Cx::current().map_or_else(asupersync::time::wall_now, |cx| {
        cx.timer_driver()
            .map_or_else(asupersync::time::wall_now, |driver| driver.now())
    });
    asupersync::time::sleep(now, duration).await;
}

/// Finite failure class for the typed timeout boundary.
///
/// `asupersync` 0.3.5 exposes only an elapsed result from `budget_timeout`.
/// Keeping that fact typed here prevents callers from interpreting an opaque
/// string as some different timer or backend failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeoutError {
    /// The explicit timeout or the caller's earlier capability deadline
    /// elapsed. Callers can inspect the `Cx` to distinguish those cases.
    Elapsed,
}

impl std::fmt::Display for TimeoutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Elapsed => formatter.write_str("timeout elapsed"),
        }
    }
}

impl std::error::Error for TimeoutError {}

/// Runs `future` with a timeout using asupersync.
pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, String>
where
    F: Future,
{
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    timeout_with_cx(&cx, duration, future).await
}

/// Runs `future` with a timeout that respects the provided `Cx`.
///
/// Cx-first timeout seam for the ft-xbnl0.2.2 migration. Call sites that
/// already thread `&Cx` should prefer this over the ambient [`timeout`]
/// which falls back to `Cx::current()` thread-local lookup.
///
/// # Cancellation semantics (ft-xbnl0.2.4 tick 328)
///
/// This function observes the cx **budget deadline** (via
/// [`asupersync::time::budget_timeout`], which bounds the effective
/// timeout by the remaining budget), but does **not** directly check
/// `cx.is_cancel_requested()`. A pre-cancelled cx with an infinite
/// budget will still wait up to the full requested `duration` before
/// returning `Err`.
///
/// Callers who need pre-cancellation short-circuit MUST add an explicit
/// `cx.checkpoint()?` (or equivalent `if cx.is_cancel_requested()` bail)
/// **before** invoking `timeout_with_cx`. Every cx-first function in
/// this crate that wraps an inner future with `timeout_with_cx` already
/// follows this pattern (e.g. `MetricsServer::start_with_cx`,
/// `start_web_server_with_cx`, `EventWaiter::wait_with_cx`).
///
/// Mid-flight direct cancellation is observable only if the wrapped future is
/// subsequently polled and checks the `Cx` itself. Neither this timeout nor
/// `sleep_with_cx` registers a direct-cancellation wake, so a suspended caller
/// that requires prompt cancellation must race against an explicit
/// cancellation signal.
pub async fn timeout_with_cx<F>(
    cx: &crate::cx::Cx,
    duration: Duration,
    future: F,
) -> Result<F::Output, String>
where
    F: Future,
{
    timeout_with_cx_typed(cx, duration, future)
        .await
        .map_err(|error| error.to_string())
}

/// Typed sibling of [`timeout_with_cx`].
///
/// Prefer this at correctness-sensitive boundaries that must not infer a
/// failure class by parsing or discarding the legacy string error.
///
/// # Errors
///
/// Returns [`TimeoutError::Elapsed`] when either the explicit duration or an
/// earlier deadline in the supplied capability budget expires.
pub(crate) async fn timeout_with_cx_typed<F>(
    cx: &crate::cx::Cx,
    duration: Duration,
    future: F,
) -> Result<F::Output, TimeoutError>
where
    F: Future,
{
    let mut future = Box::pin(future);
    let initial =
        std::future::poll_fn(|task_cx| std::task::Poll::Ready(future.as_mut().poll(task_cx))).await;
    if let std::task::Poll::Ready(output) = initial {
        return Ok(output);
    }

    asupersync::time::budget_timeout(cx, duration, future, cx_timer_now(cx))
        .await
        .map_err(|_elapsed| TimeoutError::Elapsed)
}

pub(crate) fn timer_now_with_cx(cx: &crate::cx::Cx) -> asupersync::Time {
    cx.timer_driver()
        .map_or_else(asupersync::time::wall_now, |driver| driver.now())
}

fn cx_timer_now(cx: &crate::cx::Cx) -> asupersync::Time {
    timer_now_with_cx(cx)
}

async fn spawn_blocking_in_context<T, F>(
    blocking_context: Option<(crate::cx::Cx, asupersync::cx::CapMask)>,
    runtime_handle: Option<asupersync::runtime::RuntimeHandle>,
    work: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    fn run_in_context<T, F>(
        blocking_context: Option<(crate::cx::Cx, asupersync::cx::CapMask)>,
        runtime_handle: Option<asupersync::runtime::RuntimeHandle>,
        work: F,
    ) -> T
    where
        F: FnOnce() -> T,
    {
        let _runtime_handle_guard = runtime_handle.map(install_runtime_handle_scoped);
        let _blocking_cx_guard = blocking_context
            .as_ref()
            .map(|(cx, _)| crate::cx::Cx::set_current(Some(cx.clone())));
        let _blocking_capability_guard = blocking_context
            .as_ref()
            .map(|(_, mask)| crate::cx::Cx::push_restriction(*mask));
        work()
    }

    struct CancelBlockingTaskOnDrop(Option<asupersync::runtime::blocking_pool::BlockingTaskHandle>);

    impl CancelBlockingTaskOnDrop {
        fn disarm(&mut self) {
            let _ = self.0.take();
        }
    }

    impl Drop for CancelBlockingTaskOnDrop {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                handle.cancel();
            }
        }
    }

    const PANIC_ERROR: &str = "blocking task panicked (error_code=WA-RUNTIME-BLOCKING-PANIC)";

    // Pool selection must come from the installed runtime, not from the
    // explicit operation Cx. Synthetic/request contexts intentionally carry
    // no pool handle; letting the free Asupersync helper inspect one would run
    // the supposedly blocking closure inline on the async worker.
    let blocking_pool = runtime_handle
        .as_ref()
        .and_then(asupersync::runtime::RuntimeHandle::blocking_handle);
    if let Some(blocking_pool) = blocking_pool {
        let (result_tx, result_rx) = oneshot::channel();
        let task_handle = blocking_pool.spawn(move || {
            let result = frankenterm_sigpipe::catch_recoverable(
                frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
                std::panic::AssertUnwindSafe(|| {
                    run_in_context(blocking_context, runtime_handle, work)
                }),
            );
            // Completion delivery is infrastructure cleanup, not caller work.
            // A fresh live context prevents caller cancellation from trapping
            // the receiver forever after the closure has already settled.
            let delivery_cx = crate::cx::for_request();
            if let Err(undelivered) = result_tx.send_with_cx(&delivery_cx, result) {
                // Cancellation may have dropped the receiver while the closure
                // was running. Dispose an arbitrary user result under the same
                // recovery boundary rather than letting its Drop panic escape
                // from a shared blocking-pool worker.
                let _ = frankenterm_sigpipe::catch_recoverable(
                    frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
                    std::panic::AssertUnwindSafe(|| drop(undelivered)),
                );
            }
        });
        let mut cancel_on_drop = CancelBlockingTaskOnDrop(Some(task_handle));
        let receive_cx = crate::cx::for_request();
        let result = oneshot_recv_with_cx(&receive_cx, result_rx)
            .await
            .map_err(|_| "blocking task result channel closed".to_string())?;
        cancel_on_drop.disarm();
        return result.map_err(|_| PANIC_ERROR.to_string());
    }

    // Raw runtimes and deliberately pool-less test runtimes still need an OS
    // thread. Poll the fallback handoff with no ambient Cx so Asupersync takes
    // its bounded fallback-thread path instead of its deterministic inline
    // path. The requested Cx/runtime scopes are installed only in the closure.
    let mut blocking_future = std::pin::pin!(asupersync::runtime::spawn_blocking(move || {
        run_in_context(blocking_context, runtime_handle, work)
    }));
    let fallback = std::future::poll_fn(|caller_cx| {
        let _no_ambient_cx = crate::cx::Cx::set_current(None);
        blocking_future.as_mut().poll(caller_cx)
    });
    frankenterm_sigpipe::catch_recoverable_future(
        frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
        fallback,
    )
    .await
    .map_err(|_| PANIC_ERROR.to_string())
}

/// Runs blocking work on the active runtime's blocking executor.
///
/// Returns the closure output when successful, or a stringified join/runtime
/// error when the blocking task could not complete. When an ambient Cx exists,
/// the OS-thread closure uses that exact identity and its effective capability
/// mask. Pool selection comes from the installed runtime handle, independently
/// of that operation Cx, so a synthetic or otherwise driverless explicit
/// context cannot accidentally force blocking work inline on the async
/// executor. A pool-less runtime uses Asupersync's bounded fallback-thread
/// path. The scoped closure guards are restored before the closure returns.
pub async fn spawn_blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let blocking_context = crate::cx::Cx::current().map(|cx| {
        let mask = crate::cx::effective_cap_mask(&cx);
        (cx, mask)
    });
    spawn_blocking_in_context(blocking_context, current_runtime_handle(), work).await
}

/// Typed failure from [`spawn_blocking_with_cx`].
///
/// Cancellation phase is deliberately structural rather than encoded in a
/// message so callers can distinguish a caller-requested stop from an
/// executor/runtime failure without parsing text.
#[derive(Debug, thiserror::Error)]
pub enum SpawnBlockingWithCxError {
    /// The Cx was already cancelled, so the blocking closure was not spawned.
    #[error("blocking task cancelled before spawn (kind={kind:?})")]
    CancelledBeforeSpawn {
        /// Structured cancellation kind, when the Cx carries one.
        kind: Option<crate::outcome::CancelKind>,
    },
    /// The Cx cancelled after the blocking handoff was admitted.
    ///
    /// Work still queued may be skipped by the blocking pool. A closure that
    /// already started is not preempted and continues until it returns.
    #[error("blocking task cancelled mid-flight (kind={kind:?})")]
    CancelledMidFlight {
        /// Structured cancellation kind, when the Cx carries one.
        kind: Option<crate::outcome::CancelKind>,
    },
    /// The blocking executor or join surface failed independently of Cx
    /// cancellation.
    #[error("blocking task runtime failure")]
    RuntimeFailure,
    /// The bounded cancellation watcher timer failed while the Cx itself was
    /// still live.
    #[error("blocking cancellation watcher timer failure")]
    CancellationWatcherTimerFailure,
}

impl SpawnBlockingWithCxError {
    /// Whether this error is one of the two caller-Cx cancellation phases.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::CancelledBeforeSpawn { .. } | Self::CancelledMidFlight { .. }
        )
    }
}

fn map_spawn_blocking_runtime_result<T>(
    result: Result<T, String>,
) -> Result<T, SpawnBlockingWithCxError> {
    result.map_err(|_| SpawnBlockingWithCxError::RuntimeFailure)
}

fn spawn_blocking_root_cancel_kind(cx: &crate::cx::Cx) -> Option<crate::outcome::CancelKind> {
    cx.root_cancel_cause().map(|reason| reason.kind)
}

/// br-ft-6qoxd: Cx-aware [`spawn_blocking`] that select-races the
/// blocking JoinHandle against the caller's Cx cancellation
/// watcher.
///
/// # Cancellation semantics
///
/// **Pre-cancel:** if `cx.checkpoint()` is already in error
/// before the blocking work spawns, returns `Err` immediately
/// without spawning. Callers that want a strict pre-flight gate
/// should still call `cx.checkpoint()?` themselves; this helper
/// short-circuits as a defense-in-depth on top.
///
/// **Mid-flight cancel:** if the Cx cancels while the blocking
/// work is queued or running, the await resolves with
/// [`SpawnBlockingWithCxError::CancelledMidFlight`] within ~50–100 ms
/// (the cancel-watcher checkpoints the Cx on a 50 ms cadence). The
/// blocking pool may skip work that is still queued. Once the OS-thread
/// closure has started, however, it **continues to run** until it returns
/// naturally and its result is discarded. The exact explicit Cx is installed
/// inside that closure, so cooperative code can observe cancellation between
/// operations through `Cx::current()`; cancellation cannot interrupt a
/// syscall already in progress (large SQLite scan, FTS reindex, file mmap).
/// The await just unblocks promptly.
///
/// This matches the existing select-race pattern documented at
/// `runtime_async.rs:340 / 442 / 486 / 2184 / 2277` for the
/// channel + semaphore primitives, and the
/// `distributed::race_with_cx_cancel` exemplar at tick 387.
///
/// # Trade-off
///
/// Mid-flight cancel **abandons** the result, not necessarily the work.
/// For SQLite reads that's typically fine — an abandoned scan
/// returns no data and the connection auto-rolls-back its
/// implicit transaction. For long-running writes, the abandoned
/// blocking task may still mutate state after the caller has
/// observed cancellation. Callers that need write-side abort must
/// cooperatively checkpoint the installed Cx (or layer their own cancellation
/// token) between atomic write steps and define settlement for effects already
/// committed.
pub async fn spawn_blocking_with_cx<T, F>(
    cx: &crate::cx::Cx,
    work: F,
) -> Result<T, SpawnBlockingWithCxError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    // Pre-flight guard — if the cx is already cancelled, do not
    // even spawn the blocking work. Saves a thread-pool slot +
    // matches the eager-cancel-shape callers expect.
    if cx.checkpoint().is_err() {
        return Err(SpawnBlockingWithCxError::CancelledBeforeSpawn {
            kind: spawn_blocking_root_cancel_kind(cx),
        });
    }

    use futures::future::{Either, select};

    // The explicit Cx is authoritative inside the blocking closure even when
    // this future is polled under a different ambient task. Pool selection is
    // independently derived from the installed runtime handle; the closure
    // installs the explicit identity/mask because TLS does not migrate between
    // executor and blocking-pool threads.
    let blocking_context = Some((cx.clone(), crate::cx::effective_cap_mask(cx)));
    let join_fut = std::pin::pin!(spawn_blocking_in_context(
        blocking_context,
        current_runtime_handle(),
        work,
    ));
    let cancel_watcher = std::pin::pin!(async {
        loop {
            // 50 ms poll mirrors distributed::race_with_cx_cancel
            // (tick 387). A budget deadline can make `sleep_with_cx`
            // return `Err` without first latching the cancellation bit, so
            // every wakeup must run a checkpoint. Ignoring that error used
            // to turn an expired finite budget into a non-yielding hot loop
            // until the blocking closure happened to finish.
            if sleep_with_cx(cx, std::time::Duration::from_millis(50))
                .await
                .is_err()
            {
                if cx.checkpoint().is_err() {
                    return Ok(());
                }
                return Err(());
            }
            if cx.checkpoint().is_err() {
                return Ok(());
            }
        }
    });

    match select(join_fut, cancel_watcher).await {
        Either::Left((result, _)) => map_spawn_blocking_runtime_result(result),
        Either::Right((Ok(()), _)) => Err(SpawnBlockingWithCxError::CancelledMidFlight {
            kind: spawn_blocking_root_cancel_kind(cx),
        }),
        Either::Right((Err(()), _)) => {
            Err(SpawnBlockingWithCxError::CancellationWatcherTimerFailure)
        }
    }
}

/// Receives one message from an mpsc receiver, normalized to Option semantics.
///
/// Returns:
/// - `Some(value)` when a message was received.
/// - `None` when the channel is closed.
///
/// Transitional helper retained for migration-era tests. New production
/// call-sites should prefer explicit receive semantics.
pub async fn mpsc_recv_option<T>(rx: &mut mpsc::Receiver<T>) -> Option<T> {
    {
        let cx = crate::cx::for_testing();
        rx.recv(&cx).await.ok()
    }
}

/// Sends one message through an mpsc sender using the active runtime semantics.
///
/// Transitional helper retained for migration-era tests. New production
/// call-sites should prefer explicit send semantics.
pub async fn mpsc_send<T>(tx: &mpsc::Sender<T>, value: T) -> Result<(), mpsc::SendError<T>> {
    {
        let cx = crate::cx::for_testing();
        tx.send(&cx, value).await
    }
}

/// Reserves one mpsc slot and commits `value`, returning whether delivery was
/// accepted by an active receiver.
///
/// Transitional helper retained for migration-era tests. New production
/// call-sites should prefer explicit reserve/commit semantics.
pub async fn mpsc_reserve_send<T>(tx: &mpsc::Sender<T>, value: T) -> bool {
    {
        let cx = crate::cx::for_testing();
        if let Ok(permit) = tx.reserve(&cx).await {
            permit.send(value);
            return true;
        }
        false
    }
}

/// Attempts an immediate reserve/commit send and reports whether delivery was
/// accepted.
///
/// Transitional helper retained for migration-era tests. New production
/// call-sites should prefer explicit reserve/commit semantics.
pub fn mpsc_try_reserve_send<T>(tx: &mpsc::Sender<T>, value: T) -> bool {
    if let Ok(permit) = tx.try_reserve() {
        permit.send(value);
        return true;
    }
    false
}

/// Checks whether a watch receiver has observed a new value.
///
/// Returns `false` if the channel has closed.
pub fn watch_has_changed<T>(rx: &watch::Receiver<T>) -> bool {
    rx.has_changed()
}

/// Borrows the latest watch value and clones it while marking the update as
/// consumed as required by asupersync.
pub fn watch_borrow_and_update_clone<T: Clone>(rx: &mut watch::Receiver<T>) -> T {
    rx.borrow_and_clone()
}

/// Waits until the watch receiver observes a change, abstracting the
/// `&Cx` parameter required by asupersync.
///
/// Returns `Ok(())` on change, `Err(RecvError)` if the sender was dropped.
pub async fn watch_changed<T: Send + Sync>(
    rx: &mut watch::Receiver<T>,
) -> Result<(), watch::RecvError> {
    {
        let cx = crate::cx::for_testing();
        rx.changed(&cx).await
    }
}

/// Send a value on an asupersync-backed broadcast channel.
///
/// The wrapper `Sender` acquires a `Cx` internally for the two-phase
/// reserve/commit send.
pub fn broadcast_send<T: Clone>(
    tx: &broadcast::Sender<T>,
    value: T,
) -> Result<usize, broadcast::SendError<T>> {
    tx.send(value)
}

/// Receive a value from an asupersync-backed broadcast channel.
///
/// The wrapper `Receiver` acquires a `Cx` internally for the async recv.
pub async fn broadcast_recv<T: Clone>(
    rx: &mut broadcast::Receiver<T>,
) -> Result<T, broadcast::RecvError> {
    rx.recv().await
}

/// Receive a value from a broadcast channel under an explicit `&Cx`
/// (ft-xbnl0.2.2 Cx-first helper). Prefer this over [`broadcast_recv`] in
/// call graphs that already thread `&Cx` so cancellation flows cleanly
/// through the broadcast boundary.
///
/// # Cancellation semantics
///
/// Observes **pre-cancel**: if `cx` is cancelled before this is called,
/// or if it is cancelled and something external re-wakes the recv
/// (e.g. a send), the `cx.checkpoint()` short-circuit inside asupersync's
/// `poll_recv` returns `Err(RecvError::Cancelled)` (pinned by
/// `broadcast_recv_with_cx_observes_pre_cancel`, ft-xbnl0.2.4 tick 420).
///
/// Does NOT observe **mid-flight cancel**: asupersync's broadcast
/// receiver does not register a cx-cancel-waker. A recv that has
/// already suspended on the send-side waker will NOT wake when
/// `cx.cancel_with(...)` fires afterward (pinned by
/// `broadcast_recv_with_cx_mid_flight_cancel_via_select_race_pattern`,
/// tick 434). Callers needing mid-flight cancel observability must
/// wrap this call in `futures::future::select` against a poll-sleep
/// watcher (same pattern as `DistributedHttpClient::race_with_cx_cancel`,
/// tick 387). See `docs/ft-xbnl0-2-4-completion-evidence.md` §2.6.1.
pub async fn broadcast_recv_with_cx<T: Clone>(
    cx: &crate::cx::Cx,
    rx: &mut broadcast::Receiver<T>,
) -> Result<T, broadcast::RecvError> {
    rx.recv_with_cx(cx).await
}

/// Error returned by [`broadcast_try_recv`] across runtime backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastTryRecvError {
    /// No message is currently available.
    Empty,
    /// All senders have been dropped.
    Closed,
    /// The receiver fell behind and missed one or more messages.
    Lagged(u64),
}

/// Try to receive a value from a broadcast channel without blocking.
///
/// This helper removes direct dependencies on the runtime-specific
/// `TryRecvError` type from call sites that only need the common semantics.
pub fn broadcast_try_recv<T: Clone>(
    rx: &mut broadcast::Receiver<T>,
) -> Result<T, BroadcastTryRecvError> {
    match rx.try_recv() {
        Ok(value) => Ok(value),
        Err(broadcast::TryRecvError::Empty) => Err(BroadcastTryRecvError::Empty),
        Err(broadcast::TryRecvError::Closed) => Err(BroadcastTryRecvError::Closed),
        Err(broadcast::TryRecvError::Lagged(missed_count)) => {
            Err(BroadcastTryRecvError::Lagged(missed_count))
        }
    }
}

/// Return the number of active broadcast receivers.
#[must_use]
pub fn broadcast_receiver_count<T: Clone>(tx: &broadcast::Sender<T>) -> usize {
    tx.receiver_count()
}

/// Send a value on an asupersync-backed oneshot channel.
///
/// Returns `Err(message)` if the receiver was dropped.
/// The wrapper `Sender` acquires a `Cx` internally.
pub fn oneshot_send<T>(tx: oneshot::Sender<T>, value: T) -> Result<(), String> {
    tx.send(value)
        .map_err(|_| "sending on a closed oneshot channel".to_string())
}

/// Receive from an asupersync-backed oneshot channel.
///
/// Consumes the receiver, acquires a `Cx`, and calls the asupersync `.recv()`
/// method through FrankenTerm's contained forwarding-waker boundary.
pub async fn oneshot_recv<T>(rx: oneshot::Receiver<T>) -> Result<T, String> {
    {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        oneshot_recv_with_cx(&cx, rx).await
    }
}

/// Receive from a oneshot channel under an explicit `&Cx` (ft-xbnl0.2.x
/// Cx-first primitive).
///
/// Preferred over [`oneshot_recv`] when the caller already threads
/// `&Cx` through its public API. Consumes the receiver (oneshot
/// channels fire at most once). The Cx flows into the asupersync
/// `.recv()` method instead of being pulled from thread-local state so
/// cancellation, budget, and virtual time all propagate through the
/// caller's capability context.
///
/// # Cancellation semantics
///
/// Observes **pre-cancel**: pre-cancelled cx returns Err promptly via
/// asupersync's `poll_recv` `cx.checkpoint()` short-circuit (pinned
/// by `oneshot_recv_with_cx_observes_pre_cancel`, tick 419).
///
/// Does NOT observe **mid-flight cancel**: asupersync's oneshot
/// receiver does not register a cx-cancel-waker. An already-suspended
/// recv will NOT wake when `cx.cancel_with(...)` fires afterward
/// (pinned by `oneshot_recv_with_cx_mid_flight_cancel_via_select_race_pattern`,
/// tick 433). Callers needing mid-flight cancel must wrap in
/// `futures::future::select` against a poll-sleep watcher (same
/// pattern as `DistributedHttpClient::race_with_cx_cancel`, tick 387).
/// See `docs/ft-xbnl0-2-4-completion-evidence.md` §2.6.1.
pub async fn oneshot_recv_with_cx<T>(
    cx: &crate::cx::Cx,
    rx: oneshot::Receiver<T>,
) -> Result<T, String> {
    let mut inner = rx.inner;
    // Build the containment boundary lazily. Creating, sending, or dropping a
    // channel that is never received should not pay for an extra Arc, Mutex,
    // and Waker allocation; only a receive operation can publish a caller
    // waker to the underlying primitive.
    let (forwarding, proxy_waker) = ContainedForwardingWaker::new(
        &oneshot::RECEIVER_WAKER_LOCK_POISONED_COUNT,
        &oneshot::RECEIVER_WAKER_CALLBACK_PANIC_COUNT,
        frankenterm_sigpipe::RecoverablePanicSite::CoreChannelWaker,
    );
    let mut receive = std::pin::pin!(inner.recv(cx));
    // This guard is intentionally declared after `receive`: cancellation
    // drops it first, retiring the caller waker before asupersync drops its
    // pending receive future and releases the stable proxy registration.
    let clear_on_drop = ClearContainedWakerOnDrop::new(forwarding);

    let received = std::future::poll_fn(|caller_cx| {
        if let Err(error) = clear_on_drop.register(caller_cx.waker()) {
            return std::task::Poll::Ready(Err(error));
        }
        let mut proxy_cx = std::task::Context::from_waker(&proxy_waker);
        let result = receive.as_mut().poll(&mut proxy_cx);
        match result {
            std::task::Poll::Ready(result) => {
                clear_on_drop.clear();
                std::task::Poll::Ready(Ok(result))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
    .await
    .map_err(|error| error.to_string())?;
    received.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    struct RuntimeAsyncWakeProbe {
        wake_count: std::sync::atomic::AtomicUsize,
        panic_on_wake: bool,
    }

    impl RuntimeAsyncWakeProbe {
        fn new(panic_on_wake: bool) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                wake_count: std::sync::atomic::AtomicUsize::new(0),
                panic_on_wake,
            })
        }

        fn count(&self) -> usize {
            self.wake_count.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn record_wake(&self) {
            self.wake_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(!self.panic_on_wake, "synthetic caller waker panic");
        }
    }

    impl std::task::Wake for RuntimeAsyncWakeProbe {
        fn wake(self: std::sync::Arc<Self>) {
            self.record_wake();
        }

        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.record_wake();
        }
    }

    fn probe_waker(
        panic_on_wake: bool,
    ) -> (std::sync::Arc<RuntimeAsyncWakeProbe>, std::task::Waker) {
        let probe = RuntimeAsyncWakeProbe::new(panic_on_wake);
        let waker = std::task::Waker::from(std::sync::Arc::clone(&probe));
        (probe, waker)
    }

    #[cfg(panic = "unwind")]
    struct RuntimeAsyncDropPanickingWake {
        drop_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[cfg(panic = "unwind")]
    // The no-op wake callbacks are intentional: this probe's behavior is its
    // panicking Drop, which Waker::noop cannot represent.
    #[allow(clippy::manual_noop_waker)]
    impl std::task::Wake for RuntimeAsyncDropPanickingWake {
        fn wake(self: std::sync::Arc<Self>) {}

        fn wake_by_ref(self: &std::sync::Arc<Self>) {}
    }

    #[cfg(panic = "unwind")]
    impl Drop for RuntimeAsyncDropPanickingWake {
        fn drop(&mut self) {
            self.drop_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("synthetic caller waker drop panic");
        }
    }

    #[cfg(panic = "unwind")]
    fn drop_panicking_waker() -> (
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::task::Waker,
    ) {
        let drop_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let waker = std::task::Waker::from(std::sync::Arc::new(RuntimeAsyncDropPanickingWake {
            drop_count: std::sync::Arc::clone(&drop_count),
        }));
        (drop_count, waker)
    }

    #[cfg(panic = "unwind")]
    struct MpscReentrantDropWake {
        sender: mpsc::Sender<u8>,
        completed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[cfg(panic = "unwind")]
    // The wake path is deliberately inert; Drop re-enters the mpsc sender and
    // is the behavior under test.
    #[allow(clippy::manual_noop_waker)]
    impl std::task::Wake for MpscReentrantDropWake {
        fn wake(self: std::sync::Arc<Self>) {}

        fn wake_by_ref(self: &std::sync::Arc<Self>) {}
    }

    #[cfg(panic = "unwind")]
    impl Drop for MpscReentrantDropWake {
        fn drop(&mut self) {
            let accepted = self.sender.try_send(73).is_ok();
            self.completed
                .store(accepted, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[cfg(panic = "unwind")]
    struct WatchReentrantDropWake {
        sender: watch::Sender<u8>,
        completed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[cfg(panic = "unwind")]
    // The wake path is deliberately inert; Drop re-enters the watch sender and
    // is the behavior under test.
    #[allow(clippy::manual_noop_waker)]
    impl std::task::Wake for WatchReentrantDropWake {
        fn wake(self: std::sync::Arc<Self>) {}

        fn wake_by_ref(self: &std::sync::Arc<Self>) {}
    }

    #[cfg(panic = "unwind")]
    impl Drop for WatchReentrantDropWake {
        fn drop(&mut self) {
            let accepted = self.sender.send(83).is_ok();
            self.completed
                .store(accepted, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[cfg(panic = "unwind")]
    struct BroadcastReentrantDropWake {
        sender: broadcast::Sender<u8>,
        completed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[cfg(panic = "unwind")]
    // The wake path is deliberately inert; Drop re-enters the broadcast sender
    // and is the behavior under test.
    #[allow(clippy::manual_noop_waker)]
    impl std::task::Wake for BroadcastReentrantDropWake {
        fn wake(self: std::sync::Arc<Self>) {}

        fn wake_by_ref(self: &std::sync::Arc<Self>) {}
    }

    #[cfg(panic = "unwind")]
    impl Drop for BroadcastReentrantDropWake {
        fn drop(&mut self) {
            let accepted = self.sender.send(97).is_ok();
            self.completed
                .store(accepted, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn contained_forwarding_clone_failure_is_finite_and_clears_stale_registration() {
        static LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        static CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let (forwarding, _proxy) = ContainedForwardingWaker::new(
            &LOCK_POISONED_COUNT,
            &CALLBACK_PANIC_COUNT,
            frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
        );
        let (stale_probe, stale_waker) = probe_waker(false);
        assert_eq!(forwarding.register(&stale_waker), Ok(()));

        // Negative-evidence boundary: safe `Wake` construction fixes the
        // RawWaker clone callback to Arc::clone, and this crate forbids unsafe
        // code, so no safe Waker can inject a clone panic end-to-end. This seam
        // is the exact production compare/clone/recheck path with only its
        // clone operation supplied for deterministic fault injection.
        let replacement = futures::task::noop_waker();
        let registration = forwarding.register_with(&replacement, |_| {
            panic!("synthetic caller waker clone panic");
        });
        let error = registration.expect_err("synthetic clone panic must fail registration");
        assert_eq!(error, ContainedWakerRegistrationError);
        assert_eq!(error.to_string(), "caller waker registration failed");

        forwarding.forward_one();
        assert_eq!(
            stale_probe.count(),
            0,
            "failed registration must retire rather than wake the stale caller"
        );
        assert_eq!(
            LOCK_POISONED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "caller clone callback must run without holding the slot lock"
        );
        assert_eq!(
            CALLBACK_PANIC_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "clone panic must increment the finite callback counter"
        );
    }

    #[cfg(panic = "unwind")]
    #[test]
    // The first half must exercise the owned Wake::wake path independently of
    // the borrowed wake_by_ref path tested immediately afterward.
    #[allow(clippy::waker_clone_wake)]
    fn contained_forwarding_proxy_contains_wake_and_wake_by_ref_panics() {
        static LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        static CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let (forwarding, proxy) = ContainedForwardingWaker::new(
            &LOCK_POISONED_COUNT,
            &CALLBACK_PANIC_COUNT,
            frankenterm_sigpipe::RecoverablePanicSite::CoreChannelWaker,
        );

        let (wake_probe, wake_waker) = probe_waker(true);
        forwarding
            .register(&wake_waker)
            .expect("register wake probe");
        let wake_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            proxy.clone().wake();
        }));
        assert!(wake_result.is_ok(), "proxy wake must contain caller panic");
        assert_eq!(wake_probe.count(), 1);

        let (wake_by_ref_probe, wake_by_ref_waker) = probe_waker(true);
        forwarding
            .register(&wake_by_ref_waker)
            .expect("register wake-by-ref probe");
        let wake_by_ref_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            proxy.wake_by_ref();
        }));
        assert!(
            wake_by_ref_result.is_ok(),
            "proxy wake_by_ref must contain caller panic"
        );
        assert_eq!(wake_by_ref_probe.count(), 1);
        assert_eq!(
            CALLBACK_PANIC_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            LOCK_POISONED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn contained_forwarding_replacement_contains_retired_waker_drop_panic() {
        static LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        static CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let (forwarding, _proxy) = ContainedForwardingWaker::new(
            &LOCK_POISONED_COUNT,
            &CALLBACK_PANIC_COUNT,
            frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
        );
        let (drop_count, drop_panicking) = drop_panicking_waker();
        assert_eq!(forwarding.register(&drop_panicking), Ok(()));
        drop(drop_panicking);

        let replacement = futures::task::noop_waker();
        assert_eq!(forwarding.register(&replacement), Ok(()));
        assert_eq!(
            drop_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "retired caller waker must be disposed exactly once inside quarantine"
        );
        assert_eq!(
            LOCK_POISONED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "retired caller drop callback must run without holding the slot lock"
        );
        assert_eq!(
            CALLBACK_PANIC_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "retired caller drop panic must increment the finite callback counter"
        );
        forwarding.clear();
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn contained_forwarding_state_drop_contains_residual_waker_during_outer_unwind() {
        static LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        static CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let (forwarding, proxy) = ContainedForwardingWaker::new(
            &LOCK_POISONED_COUNT,
            &CALLBACK_PANIC_COUNT,
            frankenterm_sigpipe::RecoverablePanicSite::CoreAsyncTaskJoin,
        );
        let (drop_count, drop_panicking) = drop_panicking_waker();
        assert_eq!(forwarding.register(&drop_panicking), Ok(()));
        drop(drop_panicking);

        let outer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _proxy_dropped_last = proxy;
            let _forwarding_dropped_first = forwarding;
            panic!("synthetic outer panic while dropping forwarding state");
        }));
        assert!(
            outer.is_err(),
            "the original outer panic must remain visible"
        );
        assert_eq!(
            drop_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "state destruction must quarantine one residual caller-waker drop"
        );
        assert_eq!(
            LOCK_POISONED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "state destruction must dispose the residual waker without a lock"
        );
        assert_eq!(
            CALLBACK_PANIC_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "residual caller drop panic must increment the finite callback counter"
        );
    }

    async fn await_test_signal(signal: oneshot::Receiver<()>, description: &'static str) {
        timeout(Duration::from_secs(5), oneshot_recv(signal))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
            .unwrap_or_else(|_| panic!("sender dropped before publishing {description}"));
    }

    // ft-yqd3w: the four `surface_contract_*` self-tests that used to live
    // here (entries_are_unique / replacements_are_explicit /
    // task_spawn_blocking_as_replace / catalogs_channel_bridge_modules)
    // were retired alongside SURFACE_CONTRACT_V1 / SurfaceContractEntry /
    // SurfaceDisposition. They were a self-referential audit ledger
    // checking that the ledger itself was internally consistent — with
    // the ledger gone, the tests have nothing left to anchor on. The
    // architectural invariant they were meant to fence (only
    // runtime_async.rs and cx.rs may import raw runtime primitives) is
    // pinned by `runtime_async_surface_guard::allowed_raw_runtime_files`
    // and its tests there.

    #[test]
    fn runtime_builder_current_thread_builds() {
        let rt = RuntimeBuilder::current_thread().build();
        assert!(rt.is_ok());
    }

    #[test]
    fn current_runtime_handle_tracks_install_and_clear() {
        clear_runtime_handle();
        assert!(current_runtime_handle().is_none());

        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        install_runtime_handle(runtime.inner.handle());
        assert!(current_runtime_handle().is_some());

        clear_runtime_handle();
        assert!(current_runtime_handle().is_none());
    }

    #[test]
    fn run_async_test_clears_runtime_handle_tls() {
        clear_runtime_handle();
        run_async_test(async {
            assert!(
                current_runtime_handle().is_some(),
                "block_on should install the ambient runtime handle inside the async body"
            );
        });
        assert!(
            current_runtime_handle().is_none(),
            "run_async_test should clear the ambient runtime handle after the test body finishes"
        );
    }

    #[test]
    fn runtime_builder_multi_thread_builds() {
        let rt = RuntimeBuilder::multi_thread().build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_worker_threads_chainable() {
        let rt = RuntimeBuilder::multi_thread().worker_threads(2).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_current_thread_ignores_worker_threads() {
        // current_thread doesn't support worker_threads; should not panic
        let rt = RuntimeBuilder::current_thread().worker_threads(4).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_shutdown_token_closes_admission_until_held_lease_drains() {
        let token = RuntimeShutdownToken::new();
        let lease = token.try_acquire().expect("live token must admit a lease");

        assert!(!token.is_shutdown_requested());
        assert!(
            !token.request_shutdown_and_wait(Duration::ZERO),
            "a held lease must prevent a false clean-drain result"
        );
        assert!(token.is_shutdown_requested());
        assert!(
            token.try_acquire().is_none(),
            "shutdown admission must remain one-way closed"
        );

        drop(lease);
        assert!(
            token.request_shutdown_and_wait(Duration::ZERO),
            "dropping the final lease must make the drain observably quiescent"
        );
    }

    #[test]
    fn compat_runtime_block_on_runs_future() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async { 42 });
        assert_eq!(result, 42);
    }

    #[test]
    fn compat_runtime_spawn_detached_does_not_panic() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            // Can't directly test the spawned task completes, but ensure no panic
        });
        rt.spawn_detached(async {});
    }

    #[test]
    fn pending_detached_task_does_not_keep_runtime_alive() {
        struct PendingTaskDropProbe(Arc<std::sync::atomic::AtomicBool>);

        impl Drop for PendingTaskDropProbe {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }

        // Do not call block_on here: this fixture intentionally avoids the
        // separate long-lived caller-thread TLS handle contract and isolates
        // ownership held by the spawned future itself.
        let runtime = RuntimeBuilder::multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime-cycle fixture");
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = PendingTaskDropProbe(Arc::clone(&dropped));
        runtime.spawn_detached(async move {
            let _probe = probe;
            std::future::pending::<()>().await;
        });

        drop(runtime);
        assert!(
            dropped.load(std::sync::atomic::Ordering::Acquire),
            "a pending task future must be dropped during runtime shutdown"
        );
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for async test");
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        clear_runtime_handle();
        if let Err(payload) = test_result {
            std::panic::resume_unwind(payload);
        }
    }

    /// Like `run_async_test` but spawns a dedicated thread so the test gets
    /// a pristine TLS state. Prevents interference when 25 000+ tests run
    /// in parallel and stomp each other's `ASUPERSYNC_HANDLE` thread-local.
    #[cfg(all(feature = "asupersync-runtime", unix))]
    fn run_async_test_isolated<F>(f: impl FnOnce() -> F + Send + 'static)
    where
        F: std::future::Future<Output = ()>,
    {
        let result = std::thread::Builder::new()
            .name("runtime-compat-test-isolated".into())
            .spawn(move || {
                let runtime = RuntimeBuilder::current_thread()
                    .build()
                    .expect("failed to build runtime for isolated test");
                let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(f());
                }));
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(runtime);
                }));
                clear_runtime_handle();
                if let Err(payload) = test_result {
                    std::panic::resume_unwind(payload);
                }
            })
            .expect("failed to spawn isolated test thread")
            .join();
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn sleep_completes() {
        run_async_test(async {
            let start = std::time::Instant::now();
            sleep(Duration::from_millis(10)).await;
            let elapsed = start.elapsed();
            assert!(elapsed >= Duration::from_millis(5));
        });
    }

    #[test]
    fn timeout_succeeds_before_deadline() {
        run_async_test(async {
            let result = timeout(Duration::from_secs(1), async { 99 }).await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 99);
        });
    }

    #[test]
    fn timeout_expires_returns_error() {
        run_async_test(async {
            let result = timeout(Duration::from_millis(10), async {
                sleep(Duration::from_secs(10)).await;
                42
            })
            .await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn sleep_with_cx_returns_elapsed_when_budget_is_exhausted() {
        run_async_test(async {
            let probe = crate::cx::for_testing();
            let cx = crate::cx::Cx::for_testing_with_budget(
                asupersync::Budget::new().with_deadline(cx_timer_now(&probe)),
            );

            let result = sleep_with_cx(&cx, Duration::from_secs(1)).await;
            assert!(result.is_err(), "expired budgets must short-circuit sleep");
        });
    }

    #[test]
    fn sleep_uses_active_cx_virtual_time_under_labruntime() {
        let woke = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let woke_task = std::sync::Arc::clone(&woke);
        let wall_start = std::time::Instant::now();
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(11)
                .with_auto_advance()
                .worker_count(2)
                .max_steps(10_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                sleep(Duration::from_secs(1)).await;
                woke_task.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .expect("spawn sleep task");
        runtime.scheduler.lock().schedule(task_id, 0);

        runtime.step_for_test();
        let virtual_time = runtime.run_with_auto_advance();
        let report = runtime.run_until_quiescent_with_report();

        assert!(
            virtual_time.auto_advances >= 1,
            "LabRuntime should auto-advance to the sleep deadline"
        );
        assert!(
            runtime.now() >= asupersync::Time::from_secs(1),
            "virtual time should move to the requested deadline"
        );
        assert!(
            wall_start.elapsed() < Duration::from_secs(1),
            "virtual time must not consume a real second"
        );
        assert!(
            woke.load(std::sync::atomic::Ordering::SeqCst),
            "sleep task should complete under LabRuntime"
        );
        assert!(report.oracle_report.all_passed());
        assert!(report.invariant_violations.is_empty());
    }

    fn interruptible_sleep_kind_code(kind: SleepWithCxErrorKind) -> u8 {
        match kind {
            SleepWithCxErrorKind::ContextCancelled => 1,
            SleepWithCxErrorKind::DeadlineExceeded => 2,
            SleepWithCxErrorKind::PollQuotaExhausted => 3,
            SleepWithCxErrorKind::CostBudgetExhausted => 4,
            SleepWithCxErrorKind::TimerCapacityExhausted => 5,
            SleepWithCxErrorKind::TimerDurationExceeded => 6,
            SleepWithCxErrorKind::TimerContextUnavailable => 7,
            SleepWithCxErrorKind::ContextFailure => 8,
        }
    }

    #[test]
    fn interruptible_sleep_source_uses_one_timer_and_no_worker_or_polling_loop() {
        let source = include_str!("runtime_async.rs");
        let helper_start = source
            .find("async fn sleep_with_cx_interruptible_using(")
            .expect("interruptible timer helper source");
        let helper_tail = &source[helper_start..];
        let helper_end = helper_tail
            .find("\n}\n\n/// Pause without inheriting the ambient capability budget.")
            .expect("interruptible timer helper boundary");
        let helper = &helper_tail[..helper_end];

        assert_eq!(
            helper
                .match_indices("asupersync::time::budget_sleep(")
                .count(),
            1,
            "one logical delay must construct exactly one timer future"
        );
        assert_eq!(helper.match_indices("service.try_admit()?").count(), 1);
        assert!(helper.contains("OnceCell::<()>::new()"));
        assert!(!helper.contains("spawn_blocking"));
        assert!(!helper.contains("std::thread"));
        assert!(!helper.contains("loop {"));
    }

    #[cfg(unix)]
    struct ScriptedNonblockingWriter {
        descriptor: filedescriptor::FileDescriptor,
        bytes: Vec<u8>,
        maximum_fragment: usize,
        fail_after: Option<usize>,
        write_zero: bool,
        flush_error: Option<std::io::ErrorKind>,
    }

    #[cfg(unix)]
    impl std::os::fd::AsRawFd for ScriptedNonblockingWriter {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            std::os::fd::AsRawFd::as_raw_fd(&self.descriptor)
        }
    }

    #[cfg(unix)]
    impl std::io::Write for ScriptedNonblockingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.write_zero {
                return Ok(0);
            }
            if self
                .fail_after
                .is_some_and(|fail_after| self.bytes.len() >= fail_after)
            {
                return Err(std::io::Error::other("scripted write failure"));
            }
            let permitted = self
                .fail_after
                .map_or(bytes.len(), |fail_after| {
                    fail_after.saturating_sub(self.bytes.len()).min(bytes.len())
                })
                .min(self.maximum_fragment.max(1));
            self.bytes.extend_from_slice(&bytes[..permitted]);
            Ok(permitted)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flush_error.map_or(Ok(()), |kind| {
                Err(std::io::Error::new(kind, "scripted flush failure"))
            })
        }
    }

    #[cfg(unix)]
    fn scripted_nonblocking_writer() -> ScriptedNonblockingWriter {
        let (descriptor, _peer) =
            filedescriptor::socketpair().expect("create scripted writer descriptor");
        ScriptedNonblockingWriter {
            descriptor,
            bytes: Vec::new(),
            maximum_fragment: usize::MAX,
            fail_after: None,
            write_zero: false,
            flush_error: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn nonblocking_write_receipts_preserve_exact_progress_and_flush_boundary() {
        run_async_test(async {
            let cx = crate::cx::Cx::for_testing();
            let line = b"one-complete-ndjson-line\n";

            let mut fragmented = scripted_nonblocking_writer();
            fragmented.maximum_fragment = 3;
            let receipt =
                write_all_nonblocking_with_cx(&cx, &mut fragmented, line, Duration::from_secs(1))
                    .await
                    .expect("fragmented complete write");
            assert_eq!(receipt.bytes_written(), line.len());
            assert_eq!(receipt.blocked_duration_ns(), 0);
            assert_eq!(fragmented.bytes, line);

            let mut partial = scripted_nonblocking_writer();
            partial.maximum_fragment = 5;
            partial.fail_after = Some(5);
            let error =
                write_all_nonblocking_with_cx(&cx, &mut partial, line, Duration::from_secs(1))
                    .await
                    .expect_err("scripted partial write must fail");
            assert_eq!(error.kind(), NonblockingWriteErrorKind::Write);
            assert_eq!(error.bytes_written(), 5);
            assert_eq!(partial.bytes, line[..5]);

            let mut zero = scripted_nonblocking_writer();
            zero.write_zero = true;
            let error = write_all_nonblocking_with_cx(&cx, &mut zero, line, Duration::from_secs(1))
                .await
                .expect_err("zero-progress writer must fail");
            assert_eq!(error.kind(), NonblockingWriteErrorKind::WriteZero);
            assert_eq!(error.bytes_written(), 0);

            let mut flush_failure = scripted_nonblocking_writer();
            flush_failure.flush_error = Some(std::io::ErrorKind::BrokenPipe);
            let error = write_all_nonblocking_with_cx(
                &cx,
                &mut flush_failure,
                line,
                Duration::from_secs(1),
            )
            .await
            .expect_err("flush failure is not a delivery acknowledgement");
            assert_eq!(error.kind(), NonblockingWriteErrorKind::Flush);
            assert_eq!(error.bytes_written(), line.len());
            assert_eq!(flush_failure.bytes, line);
        });
    }

    #[cfg(unix)]
    #[test]
    fn nonblocking_write_source_has_one_lazy_timer_and_no_blocking_worker() {
        let source = include_str!("runtime_async.rs");
        let helper_start = source
            .find("pub async fn write_all_nonblocking_with_cx<W>(")
            .expect("nonblocking output helper source");
        let helper_tail = &source[helper_start..];
        let helper_end = helper_tail
            .find("\n}\n\n/// Pause without inheriting the ambient capability budget.")
            .expect("nonblocking output helper boundary");
        let helper = &helper_tail[..helper_end];

        assert_eq!(helper.match_indices("cx.register_io(").count(), 1);
        assert_eq!(
            helper
                .match_indices("asupersync::time::budget_sleep(")
                .count(),
            1
        );
        assert!(helper.contains("active_context_matches"));
        assert!(!helper.contains("spawn_blocking"));
        assert!(!helper.contains("std::thread"));
    }

    #[cfg(unix)]
    #[test]
    fn nonblocking_write_rejects_precancellation_without_output() {
        run_async_test(async {
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::Shutdown,
                Some("pre-cancel nonblocking output"),
            );
            let mut writer = scripted_nonblocking_writer();
            let error = write_all_nonblocking_with_cx(
                &cx,
                &mut writer,
                b"must-not-write\n",
                Duration::from_secs(1),
            )
            .await
            .expect_err("pre-cancelled output must fail before its first byte");
            assert_eq!(error.kind(), NonblockingWriteErrorKind::ContextCancelled);
            assert_eq!(error.bytes_written(), 0);
            assert!(writer.bytes.is_empty());
        });
    }

    #[cfg(all(feature = "asupersync-runtime", unix))]
    #[test]
    fn nonblocking_write_has_an_exact_finite_output_timeout() {
        run_async_test_isolated(|| async {
            let (mut writer, _reader) =
                filedescriptor::socketpair().expect("create blocked timeout socket pair");
            writer
                .set_non_blocking(true)
                .expect("set timeout socket nonblocking");
            let fill = [0_u8; 16 * 1024];
            loop {
                match std::io::Write::write(&mut writer, &fill) {
                    Ok(0) => panic!("socket reported zero progress while filling its buffer"),
                    Ok(_written) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("failed to fill timeout socket: {error}"),
                }
            }

            let cx = crate::cx::Cx::current()
                .expect("runtime task must expose its reactor-capable context");
            let error =
                write_all_nonblocking_with_cx(&cx, &mut writer, b"x", Duration::from_millis(10))
                    .await
                    .expect_err("finite output bound must settle without readiness");
            assert_eq!(error.kind(), NonblockingWriteErrorKind::OutputTimeout);
            assert_eq!(error.bytes_written(), 0);
        });
    }

    #[cfg(all(feature = "asupersync-runtime", unix))]
    #[test]
    fn blocked_nonblocking_write_wakes_directly_on_cx_cancellation() {
        run_async_test_isolated(|| async {
            let (mut writer, mut reader) =
                filedescriptor::socketpair().expect("create blocked output socket pair");
            writer
                .set_non_blocking(true)
                .expect("set output socket nonblocking");
            let fill = [0_u8; 16 * 1024];
            loop {
                match std::io::Write::write(&mut writer, &fill) {
                    Ok(0) => panic!("socket reported zero progress while filling its buffer"),
                    Ok(_written) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("failed to fill output socket: {error}"),
                }
            }

            let cx = crate::cx::Cx::current()
                .expect("runtime task must expose its reactor-capable context");
            let cancel_cx = cx.clone();
            let fallback_reader = std::thread::Builder::new()
                .name("nonblocking-write-cancel-fallback".to_string())
                .spawn(move || {
                    std::thread::sleep(Duration::from_millis(10));
                    cancel_cx.cancel_with(
                        crate::outcome::CancelKind::Shutdown,
                        Some("cancel blocked output"),
                    );
                    // This delayed read makes the test finite even if the
                    // direct cancellation wake regresses. In that case output
                    // wins after cancellation and the assertions below fail.
                    std::thread::sleep(Duration::from_millis(500));
                    let mut drain = [0_u8; 16 * 1024];
                    let _ = std::io::Read::read(&mut reader, &mut drain);
                })
                .expect("spawn finite cancellation fallback");

            let started_at = std::time::Instant::now();
            let error =
                write_all_nonblocking_with_cx(&cx, &mut writer, b"x", Duration::from_secs(1))
                    .await
                    .expect_err("Cx cancellation must preempt blocked output");
            let elapsed = started_at.elapsed();
            fallback_reader
                .join()
                .expect("join finite cancellation fallback");
            assert_eq!(error.kind(), NonblockingWriteErrorKind::ContextCancelled);
            assert_eq!(error.bytes_written(), 0);
            assert!(
                error.cancellation_latency_upper_bound_ns()
                    <= u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
                "the recorded cancellation latency must remain a conservative observed bound"
            );
            assert!(
                elapsed < Duration::from_millis(400),
                "cancellation waited for descriptor readiness instead of its direct Cx wake: {elapsed:?}"
            );
        });
    }

    #[test]
    fn interruptible_sleep_rejects_precancel_without_arming_a_timer() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel interruptible sleep"),
            );

            let error = sleep_with_cx_interruptible(&cx, Duration::from_secs(60))
                .await
                .expect_err("pre-cancelled sleep must not arm or wait");
            assert_eq!(error.kind(), SleepWithCxErrorKind::ContextCancelled);
            assert!(error.is_cancelled());
        });
    }

    #[test]
    fn interruptible_sleep_timer_service_fails_closed_at_capacity_and_cleans_shutdown() {
        let service = std::sync::Arc::new(InterruptibleTimerService::new(2));
        let contexts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let saturated = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x7A11_5A70)
                .worker_count(2)
                .max_steps(20_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);

        for _ in 0..3 {
            let service_task = std::sync::Arc::clone(&service);
            let contexts_task = std::sync::Arc::clone(&contexts);
            let saturated_task = std::sync::Arc::clone(&saturated);
            let cancelled_task = std::sync::Arc::clone(&cancelled);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    let cx = crate::cx::Cx::current()
                        .expect("LabRuntime task must expose its owning context");
                    contexts_task
                        .lock()
                        .expect("record saturation context")
                        .push(cx.clone());
                    let error = sleep_with_cx_interruptible_using(
                        &cx,
                        Duration::from_secs(60),
                        &service_task,
                    )
                    .await
                    .expect_err("capacity test never advances the timer deadline");
                    match error.kind() {
                        SleepWithCxErrorKind::TimerCapacityExhausted => {
                            saturated_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        SleepWithCxErrorKind::ContextCancelled => {
                            cancelled_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        other => panic!("unexpected capacity-test outcome: {other:?}"),
                    }
                })
                .expect("spawn bounded timer saturation task");
            runtime.scheduler.lock().schedule(task_id, 0);
        }

        runtime.run_until_idle();
        let saturated_metrics = service.snapshot();
        assert_eq!(saturated_metrics.capacity, 2);
        assert_eq!(saturated_metrics.active, 2);
        assert_eq!(saturated_metrics.admissions, 2);
        assert_eq!(saturated_metrics.saturations, 1);
        assert_eq!(saturated.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(runtime.pending_timer_count(), 2);

        for cx in contexts.lock().expect("cancel saturation contexts").iter() {
            cx.cancel_with(
                crate::outcome::CancelKind::Shutdown,
                Some("bounded timer saturation shutdown"),
            );
        }
        runtime.run_until_idle();

        let cleaned_metrics = service.snapshot();
        assert_eq!(
            cleaned_metrics,
            InterruptibleTimerMetrics {
                capacity: 2,
                active: 0,
                admissions: 2,
                saturations: 1,
                duration_refusals: 0,
                context_refusals: 0,
                cancellations: 2,
                deadline_expirations: 0,
                budget_exhaustions: 0,
                context_failures: 0,
                wake_completions: 0,
                stale_wakeups: 0,
                shutdown_cleanups: 2,
                max_wake_latency_ns: 0,
            }
        );
        assert_eq!(cancelled.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(runtime.pending_timer_count(), 0);
    }

    #[test]
    fn interruptible_sleep_rejects_a_nonactive_or_timerless_context_before_admission() {
        run_async_test(async {
            let explicit = crate::cx::for_testing();
            let service = InterruptibleTimerService::new(1);
            let error =
                sleep_with_cx_interruptible_using(&explicit, Duration::from_millis(1), &service)
                    .await
                    .expect_err(
                        "timerless context must not fall through to a different timer domain",
                    );
            assert_eq!(error.kind(), SleepWithCxErrorKind::TimerContextUnavailable);
            assert_eq!(service.snapshot().context_refusals, 1);
            assert_eq!(service.snapshot().admissions, 0);
            assert_eq!(service.snapshot().active, 0);
        });
    }

    #[test]
    fn interruptible_sleep_handles_zero_subslice_and_exact_long_delays() {
        let cases = [
            Duration::ZERO,
            Duration::from_millis(501),
            INTERRUPTIBLE_TIMER_MAX_DELAY,
        ];

        for (case_index, duration) in cases.into_iter().enumerate() {
            let service = std::sync::Arc::new(InterruptibleTimerService::new(1));
            let service_task = std::sync::Arc::clone(&service);
            let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed_task = std::sync::Arc::clone(&completed);
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(
                    0x7A11_0000 + u64::try_from(case_index).expect("case index fits u64"),
                )
                .with_auto_advance()
                .worker_count(2)
                .max_steps(20_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    let cx = crate::cx::Cx::current()
                        .expect("LabRuntime task must expose its owning context");
                    sleep_with_cx_interruptible_using(&cx, duration, &service_task)
                        .await
                        .expect("infinite-budget interruptible sleep must complete");
                    completed_task.store(true, std::sync::atomic::Ordering::SeqCst);
                })
                .expect("spawn interruptible timer-boundary task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            let oracle_report = runtime.run_until_quiescent_with_report();

            assert!(
                completed.load(std::sync::atomic::Ordering::SeqCst),
                "duration case {duration:?} did not complete"
            );
            assert_eq!(
                runtime.pending_timer_count(),
                0,
                "duration case {duration:?} leaked its timer registration"
            );
            assert!(
                report.auto_advances >= u64::from(!duration.is_zero()),
                "non-zero duration case {duration:?} must advance virtual time"
            );
            let metrics = service.snapshot();
            assert_eq!(metrics.active, 0);
            assert_eq!(metrics.saturations, 0);
            assert_eq!(metrics.duration_refusals, 0);
            assert_eq!(metrics.context_refusals, 0);
            assert_eq!(metrics.cancellations, 0);
            assert_eq!(metrics.deadline_expirations, 0);
            assert_eq!(metrics.budget_exhaustions, 0);
            assert_eq!(metrics.context_failures, 0);
            assert_eq!(metrics.stale_wakeups, 0);
            assert_eq!(metrics.shutdown_cleanups, 0);
            assert_eq!(metrics.admissions, u64::from(!duration.is_zero()));
            assert_eq!(metrics.wake_completions, u64::from(!duration.is_zero()));
            assert_eq!(metrics.max_wake_latency_ns, 0);
            if !duration.is_zero() {
                let duration_ns = u64::try_from(duration.as_nanos())
                    .expect("admitted timer duration fits the Time domain");
                assert!(
                    runtime.now() >= asupersync::Time::from_nanos(duration_ns),
                    "duration case {duration:?} woke before its exact deadline"
                );
            }
            assert!(oracle_report.oracle_report.all_passed());
            assert!(oracle_report.invariant_violations.is_empty());
        }
    }

    #[test]
    fn interruptible_sleep_refuses_clock_extreme_instead_of_waking_early() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let service = InterruptibleTimerService::new(1);
            let error = sleep_with_cx_interruptible_using(&cx, Duration::MAX, &service)
                .await
                .expect_err("out-of-range timer must fail before registration");
            assert_eq!(error.kind(), SleepWithCxErrorKind::TimerDurationExceeded);
            assert!(!error.is_cancelled());
            assert_eq!(
                service.snapshot(),
                InterruptibleTimerMetrics {
                    capacity: 1,
                    active: 0,
                    admissions: 0,
                    saturations: 0,
                    duration_refusals: 1,
                    context_refusals: 0,
                    cancellations: 0,
                    deadline_expirations: 0,
                    budget_exhaustions: 0,
                    context_failures: 0,
                    wake_completions: 0,
                    stale_wakeups: 0,
                    shutdown_cleanups: 0,
                    max_wake_latency_ns: 0,
                }
            );
        });
    }

    #[test]
    fn interruptible_sleep_saturates_near_the_clock_ceiling_without_early_wake() {
        let service = std::sync::Arc::new(InterruptibleTimerService::new(1));
        let service_task = std::sync::Arc::clone(&service);
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_task = std::sync::Arc::clone(&completed);
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x7A11_CE11)
                .with_auto_advance()
                .worker_count(2)
                .max_steps(20_000),
        );
        let start = asupersync::Time::from_nanos(u64::MAX - 1_000_000);
        runtime.advance_time_to(start);
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                let cx = crate::cx::Cx::current()
                    .expect("LabRuntime task must expose its owning context");
                sleep_with_cx_interruptible_using(&cx, Duration::from_secs(1), &service_task)
                    .await
                    .expect("clock-ceiling saturation must retain one exact timer");
                completed_task.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .expect("spawn clock-ceiling interruptible sleep");
        runtime.scheduler.lock().schedule(task_id, 0);

        runtime.run_until_idle();
        assert!(!completed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(runtime.pending_timer_count(), 1);

        let virtual_report = runtime.run_with_auto_advance();
        let oracle_report = runtime.run_until_quiescent_with_report();
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(runtime.now(), asupersync::Time::MAX);
        assert_eq!(runtime.pending_timer_count(), 0);
        assert!(virtual_report.auto_advances >= 1);
        assert_eq!(service.snapshot().wake_completions, 1);
        assert_eq!(service.snapshot().max_wake_latency_ns, 0);
        assert!(oracle_report.oracle_report.all_passed());
        assert!(oracle_report.invariant_violations.is_empty());
    }

    #[test]
    fn interruptible_sleep_classifies_capability_deadline_exactly() {
        let service = std::sync::Arc::new(InterruptibleTimerService::new(1));
        let service_task = std::sync::Arc::clone(&service);
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let observed_task = std::sync::Arc::clone(&observed);
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x7A11_D1E0)
                .with_auto_advance()
                .worker_count(2)
                .max_steps(20_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let budget = asupersync::Budget::new().with_deadline(asupersync::Time::from_millis(5));
        let (task_id, _handle) = runtime
            .state
            .create_task(region, budget, async move {
                let cx = crate::cx::Cx::current()
                    .expect("LabRuntime task must expose its owning context");
                let error =
                    sleep_with_cx_interruptible_using(&cx, Duration::from_secs(60), &service_task)
                        .await
                        .expect_err("capability deadline must interrupt the longer sleep");
                observed_task.store(
                    interruptible_sleep_kind_code(error.kind()),
                    std::sync::atomic::Ordering::SeqCst,
                );
            })
            .expect("spawn deadline-bound interruptible sleep");
        runtime.scheduler.lock().schedule(task_id, 0);

        let report = runtime.run_with_auto_advance();
        let oracle_report = runtime.run_until_quiescent_with_report();

        assert_eq!(
            observed.load(std::sync::atomic::Ordering::SeqCst),
            interruptible_sleep_kind_code(SleepWithCxErrorKind::DeadlineExceeded)
        );
        assert_eq!(runtime.pending_timer_count(), 0);
        assert!(report.auto_advances >= 1);
        let metrics = service.snapshot();
        assert_eq!(metrics.active, 0);
        assert_eq!(metrics.admissions, 1);
        assert_eq!(metrics.deadline_expirations, 1);
        assert_eq!(metrics.cancellations, 0);
        assert_eq!(metrics.budget_exhaustions, 0);
        assert_eq!(metrics.context_failures, 0);
        assert!(oracle_report.oracle_report.all_passed());
        assert!(oracle_report.invariant_violations.is_empty());
    }

    #[test]
    fn interruptible_sleep_classifies_midflight_poll_quota_exhaustion_exactly() {
        let service = std::sync::Arc::new(InterruptibleTimerService::new(1));
        let service_task = std::sync::Arc::clone(&service);
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let observed_task = std::sync::Arc::clone(&observed);
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x7A11_B0D6)
                .worker_count(2)
                .max_steps(20_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        // LabRuntime consumes one poll credit immediately before every task
        // poll. The first credit arms the timer; the second reschedule below
        // exhausts the quota and wakes the cancellation side of the race.
        let budget = asupersync::Budget::new().with_poll_quota(2);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, budget, async move {
                let cx = crate::cx::Cx::current()
                    .expect("LabRuntime task must expose its owning context");
                let error =
                    sleep_with_cx_interruptible_using(&cx, Duration::from_secs(60), &service_task)
                        .await
                        .expect_err("the second scheduler poll must exhaust the capability quota");
                observed_task.store(
                    interruptible_sleep_kind_code(error.kind()),
                    std::sync::atomic::Ordering::SeqCst,
                );
            })
            .expect("spawn poll-quota-bound interruptible sleep");
        runtime.scheduler.lock().schedule(task_id, 0);

        runtime.run_until_idle();
        assert_eq!(runtime.pending_timer_count(), 1);
        assert_eq!(service.snapshot().active, 1);

        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_idle();

        assert_eq!(
            observed.load(std::sync::atomic::Ordering::SeqCst),
            interruptible_sleep_kind_code(SleepWithCxErrorKind::PollQuotaExhausted)
        );
        assert_eq!(runtime.pending_timer_count(), 0);
        assert_eq!(
            service.snapshot(),
            InterruptibleTimerMetrics {
                capacity: 1,
                active: 0,
                admissions: 1,
                saturations: 0,
                duration_refusals: 0,
                context_refusals: 0,
                cancellations: 0,
                deadline_expirations: 0,
                budget_exhaustions: 1,
                context_failures: 0,
                wake_completions: 0,
                stale_wakeups: 0,
                shutdown_cleanups: 0,
                max_wake_latency_ns: 0,
            }
        );
    }

    #[test]
    fn interruptible_sleep_classifies_admitted_cost_budget_cancellation_exactly() {
        let service = std::sync::Arc::new(InterruptibleTimerService::new(1));
        let service_task = std::sync::Arc::clone(&service);
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let observed_task = std::sync::Arc::clone(&observed);
        let task_cx = std::sync::Arc::new(std::sync::Mutex::new(None));
        let task_cx_writer = std::sync::Arc::clone(&task_cx);
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x7A11_C057)
                .worker_count(2)
                .max_steps(20_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                let cx = crate::cx::Cx::current()
                    .expect("LabRuntime task must expose its owning context");
                *task_cx_writer
                    .lock()
                    .expect("publish cost-budget task context") = Some(cx.clone());
                let error =
                    sleep_with_cx_interruptible_using(&cx, Duration::from_secs(60), &service_task)
                        .await
                        .expect_err("cost-budget cancellation must interrupt an admitted timer");
                observed_task.store(
                    interruptible_sleep_kind_code(error.kind()),
                    std::sync::atomic::Ordering::SeqCst,
                );
            })
            .expect("spawn cost-budget-bound interruptible sleep");
        runtime.scheduler.lock().schedule(task_id, 0);

        runtime.run_until_idle();
        assert_eq!(runtime.pending_timer_count(), 1);
        assert_eq!(service.snapshot().active, 1);

        task_cx
            .lock()
            .expect("read cost-budget task context")
            .as_ref()
            .expect("cost-budget task published its context")
            .cancel_with(
                crate::outcome::CancelKind::CostBudget,
                Some("synthetic admitted cost-budget exhaustion"),
            );
        runtime.run_until_idle();

        assert_eq!(
            observed.load(std::sync::atomic::Ordering::SeqCst),
            interruptible_sleep_kind_code(SleepWithCxErrorKind::CostBudgetExhausted)
        );
        assert_eq!(runtime.pending_timer_count(), 0);
        assert_eq!(
            service.snapshot(),
            InterruptibleTimerMetrics {
                capacity: 1,
                active: 0,
                admissions: 1,
                saturations: 0,
                duration_refusals: 0,
                context_refusals: 0,
                cancellations: 0,
                deadline_expirations: 0,
                budget_exhaustions: 1,
                context_failures: 0,
                wake_completions: 0,
                stale_wakeups: 0,
                shutdown_cleanups: 0,
                max_wake_latency_ns: 0,
            }
        );
    }

    #[test]
    fn interruptible_sleep_cancel_wins_at_the_ready_timer_boundary() {
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let observed_task = std::sync::Arc::clone(&observed);
        let task_cx = std::sync::Arc::new(std::sync::Mutex::new(None));
        let task_cx_writer = std::sync::Arc::clone(&task_cx);
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x7A11_CA11)
                .worker_count(2)
                .max_steps(20_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                let cx = crate::cx::Cx::current()
                    .expect("LabRuntime task must expose its owning context");
                *task_cx_writer.lock().expect("publish timer task context") = Some(cx.clone());
                let result = sleep_with_cx_interruptible(&cx, Duration::from_millis(5)).await;
                let code = result.map_or_else(
                    |error| interruptible_sleep_kind_code(error.kind()),
                    |()| u8::MAX,
                );
                observed_task.store(code, std::sync::atomic::Ordering::SeqCst);
            })
            .expect("spawn cancel-vs-ready interruptible sleep");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_idle();
        assert_eq!(runtime.pending_timer_count(), 1);

        runtime.advance_time(5_000_000);
        task_cx
            .lock()
            .expect("read timer task context")
            .as_ref()
            .expect("timer task published its context")
            .cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel exactly at timer deadline"),
            );
        runtime.run_until_idle();

        assert_eq!(
            observed.load(std::sync::atomic::Ordering::SeqCst),
            interruptible_sleep_kind_code(SleepWithCxErrorKind::ContextCancelled),
            "post-timer checkpoint must make cancellation win the ready race"
        );
        assert_eq!(runtime.pending_timer_count(), 0);
    }

    #[test]
    fn interruptible_sleep_ignores_backward_clock_jump_and_keeps_exact_deadline() {
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_task = std::sync::Arc::clone(&completed);
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x7A11_C10C)
                .worker_count(2)
                .max_steps(20_000),
        );
        runtime.advance_time(1_000_000_000);
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                let cx = crate::cx::Cx::current()
                    .expect("LabRuntime task must expose its owning context");
                sleep_with_cx_interruptible(&cx, Duration::from_secs(1))
                    .await
                    .expect("backward clock attempt must not fail the timer");
                completed_task.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .expect("spawn backward-clock interruptible sleep");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_idle();
        assert_eq!(runtime.pending_timer_count(), 1);

        runtime.advance_time_to(asupersync::Time::from_millis(500));
        assert_eq!(runtime.now(), asupersync::Time::from_secs(1));
        assert_eq!(runtime.pending_timer_count(), 1);

        let virtual_report = runtime.run_with_auto_advance();
        let oracle_report = runtime.run_until_quiescent_with_report();
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(runtime.now() >= asupersync::Time::from_secs(2));
        assert_eq!(runtime.pending_timer_count(), 0);
        assert!(virtual_report.auto_advances >= 1);
        assert!(oracle_report.oracle_report.all_passed());
        assert!(oracle_report.invariant_violations.is_empty());
    }

    #[test]
    fn interruptible_sleep_scales_one_registration_per_follower_without_rearming() {
        for &follower_count in &[1_usize, 50, 200, 1_000] {
            let follower_count_u64 =
                u64::try_from(follower_count).expect("follower count fits u64");
            let service = std::sync::Arc::new(InterruptibleTimerService::new(follower_count));
            let contexts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let settled = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(0x7A11_5000 + follower_count_u64)
                    .worker_count(4)
                    .trace_capacity(follower_count.saturating_mul(32).saturating_add(1_024))
                    .max_steps(200_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let mut task_ids = Vec::with_capacity(follower_count);

            for _ in 0..follower_count {
                let service_task = std::sync::Arc::clone(&service);
                let contexts_task = std::sync::Arc::clone(&contexts);
                let settled_task = std::sync::Arc::clone(&settled);
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, asupersync::Budget::INFINITE, async move {
                        let cx = crate::cx::Cx::current()
                            .expect("LabRuntime task must expose its owning context");
                        contexts_task
                            .lock()
                            .expect("record follower context")
                            .push(cx.clone());
                        let error = sleep_with_cx_interruptible_using(
                            &cx,
                            Duration::from_secs(24 * 60 * 60),
                            &service_task,
                        )
                        .await
                        .expect_err("shutdown must cancel every synthetic follower");
                        assert_eq!(error.kind(), SleepWithCxErrorKind::ContextCancelled);
                        settled_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    })
                    .expect("spawn synthetic interruptible follower");
                runtime.scheduler.lock().schedule(task_id, 0);
                task_ids.push(task_id);
            }

            let initial_scheduler_steps = runtime.run_until_idle();
            assert_eq!(
                contexts.lock().expect("read follower contexts").len(),
                follower_count
            );
            assert_eq!(
                runtime.pending_timer_count(),
                follower_count,
                "each follower must own exactly one timer registration"
            );
            let armed_metrics = service.snapshot();
            assert_eq!(armed_metrics.capacity, follower_count);
            assert_eq!(armed_metrics.active, follower_count);
            assert_eq!(armed_metrics.admissions, follower_count_u64);
            assert_eq!(armed_metrics.saturations, 0);
            assert_eq!(armed_metrics.duration_refusals, 0);
            assert_eq!(armed_metrics.context_refusals, 0);
            assert_eq!(armed_metrics.cancellations, 0);
            assert_eq!(armed_metrics.deadline_expirations, 0);
            assert_eq!(armed_metrics.budget_exhaustions, 0);
            assert_eq!(armed_metrics.context_failures, 0);
            assert_eq!(armed_metrics.wake_completions, 0);
            assert_eq!(armed_metrics.stale_wakeups, 0);
            assert_eq!(armed_metrics.shutdown_cleanups, 0);
            assert_eq!(armed_metrics.max_wake_latency_ns, 0);
            assert!(
                initial_scheduler_steps >= follower_count_u64,
                "initial scheduler counter must poll every follower at least once"
            );

            let mut repeated_poll_steps = 0_u64;
            for _ in 0..3 {
                for &task_id in &task_ids {
                    runtime.scheduler.lock().schedule(task_id, 0);
                }
                repeated_poll_steps = repeated_poll_steps.saturating_add(runtime.run_until_idle());
                assert_eq!(
                    runtime.pending_timer_count(),
                    follower_count,
                    "repeated polling must update neither timer count nor registration multiplicity"
                );
            }
            let repoll_metrics = service.snapshot();
            assert_eq!(repoll_metrics.admissions, follower_count_u64);
            assert_eq!(repoll_metrics.active, follower_count);
            assert_eq!(repoll_metrics.saturations, 0);
            assert_eq!(repoll_metrics.duration_refusals, 0);
            assert_eq!(repoll_metrics.context_refusals, 0);
            assert_eq!(repoll_metrics.cancellations, 0);
            assert_eq!(repoll_metrics.deadline_expirations, 0);
            assert_eq!(repoll_metrics.budget_exhaustions, 0);
            assert_eq!(repoll_metrics.context_failures, 0);
            assert_eq!(repoll_metrics.wake_completions, 0);
            assert_eq!(
                repoll_metrics.stale_wakeups,
                follower_count_u64.saturating_mul(3),
                "each injected nonterminal re-poll must be counted without rearming"
            );
            assert_eq!(repoll_metrics.shutdown_cleanups, 0);
            assert_eq!(repoll_metrics.max_wake_latency_ns, 0);
            assert!(
                repeated_poll_steps >= follower_count_u64.saturating_mul(3),
                "scheduler-step counter must record every injected re-poll"
            );

            for cx in contexts.lock().expect("cancel follower contexts").iter() {
                cx.cancel_with(
                    crate::outcome::CancelKind::Shutdown,
                    Some("synthetic follower shutdown"),
                );
            }
            let shutdown_scheduler_steps = runtime.run_until_idle();

            assert_eq!(
                settled.load(std::sync::atomic::Ordering::SeqCst),
                follower_count,
                "every cancelled follower must settle"
            );
            assert_eq!(
                runtime.pending_timer_count(),
                0,
                "shutdown must deregister every losing timer"
            );
            let cleaned_metrics = service.snapshot();
            assert_eq!(cleaned_metrics.active, 0);
            assert_eq!(cleaned_metrics.admissions, follower_count_u64);
            assert_eq!(cleaned_metrics.saturations, 0);
            assert_eq!(cleaned_metrics.duration_refusals, 0);
            assert_eq!(cleaned_metrics.context_refusals, 0);
            assert_eq!(cleaned_metrics.cancellations, follower_count_u64);
            assert_eq!(cleaned_metrics.shutdown_cleanups, follower_count_u64);
            assert_eq!(cleaned_metrics.wake_completions, 0);
            assert_eq!(cleaned_metrics.deadline_expirations, 0);
            assert_eq!(cleaned_metrics.budget_exhaustions, 0);
            assert_eq!(cleaned_metrics.context_failures, 0);
            assert_eq!(
                cleaned_metrics.stale_wakeups,
                follower_count_u64.saturating_mul(3)
            );
            assert_eq!(cleaned_metrics.max_wake_latency_ns, 0);
            assert!(
                shutdown_scheduler_steps >= follower_count_u64,
                "shutdown wake counter must schedule every admitted follower"
            );

            let trace = runtime.trace().snapshot();
            let timer_scheduled = trace
                .iter()
                .filter(|event| event.kind == asupersync::trace::TraceEventKind::TimerScheduled)
                .count();
            let timer_cancelled = trace
                .iter()
                .filter(|event| event.kind == asupersync::trace::TraceEventKind::TimerCancelled)
                .count();
            let wake_events = trace
                .iter()
                .filter(|event| event.kind == asupersync::trace::TraceEventKind::Wake)
                .count();
            assert_eq!(timer_scheduled, follower_count);
            assert_eq!(timer_cancelled, follower_count);
            assert!(
                wake_events >= follower_count,
                "trace wake counter must include every shutdown cancellation"
            );
            let report = runtime.run_until_quiescent_with_report();
            assert!(report.oracle_report.all_passed());
            assert!(report.invariant_violations.is_empty());
        }
    }

    #[test]
    fn interruptible_sleep_wakes_one_thousand_same_deadline_followers_without_starvation() {
        const FOLLOWER_COUNT: usize = 1_000;
        const FOLLOWER_COUNT_U64: u64 = 1_000;
        let service = std::sync::Arc::new(InterruptibleTimerService::new(FOLLOWER_COUNT));
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x7A11_FA17)
                .worker_count(4)
                .trace_capacity(FOLLOWER_COUNT * 24)
                .max_steps(200_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);

        for _ in 0..FOLLOWER_COUNT {
            let service_task = std::sync::Arc::clone(&service);
            let completed_task = std::sync::Arc::clone(&completed);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    let cx = crate::cx::Cx::current()
                        .expect("LabRuntime task must expose its owning context");
                    sleep_with_cx_interruptible_using(&cx, Duration::from_millis(1), &service_task)
                        .await
                        .expect("same-deadline follower must wake normally");
                    completed_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
                .expect("spawn same-deadline interruptible follower");
            runtime.scheduler.lock().schedule(task_id, 0);
        }

        let admission_steps = runtime.run_until_idle();
        assert!(admission_steps >= FOLLOWER_COUNT_U64);
        assert_eq!(runtime.pending_timer_count(), FOLLOWER_COUNT);
        assert_eq!(service.snapshot().active, FOLLOWER_COUNT);

        runtime.advance_time(1_000_000);
        runtime.run_until_idle();
        let report = runtime.run_until_quiescent_with_report();
        assert_eq!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            FOLLOWER_COUNT
        );
        assert_eq!(runtime.pending_timer_count(), 0);
        let metrics = service.snapshot();
        assert_eq!(metrics.active, 0);
        assert_eq!(metrics.admissions, FOLLOWER_COUNT_U64);
        assert_eq!(metrics.saturations, 0);
        assert_eq!(metrics.duration_refusals, 0);
        assert_eq!(metrics.context_refusals, 0);
        assert_eq!(metrics.wake_completions, FOLLOWER_COUNT_U64);
        assert_eq!(metrics.cancellations, 0);
        assert_eq!(metrics.deadline_expirations, 0);
        assert_eq!(metrics.budget_exhaustions, 0);
        assert_eq!(metrics.context_failures, 0);
        assert_eq!(metrics.stale_wakeups, 0);
        assert_eq!(metrics.shutdown_cleanups, 0);
        assert_eq!(metrics.max_wake_latency_ns, 0);
        let trace = runtime.trace().snapshot();
        let timer_scheduled = trace
            .iter()
            .filter(|event| event.kind == asupersync::trace::TraceEventKind::TimerScheduled)
            .count();
        let timer_fired = trace
            .iter()
            .filter(|event| event.kind == asupersync::trace::TraceEventKind::TimerFired)
            .count();
        assert_eq!(timer_scheduled, FOLLOWER_COUNT);
        assert_eq!(timer_fired, FOLLOWER_COUNT);
        // LabRuntime records the timer lifecycle directly; its TaskWaker
        // schedules the task without emitting a separate Wake trace event.
        // Exact TimerFired counts plus the service completion counter are the
        // causal proof that every same-deadline follower was resumed.
        assert!(report.oracle_report.all_passed());
        assert!(report.invariant_violations.is_empty());
    }

    #[test]
    fn timeout_uses_active_cx_budget_under_labruntime() {
        let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let timed_out_task = std::sync::Arc::clone(&timed_out);
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(19)
                .with_auto_advance()
                .worker_count(2)
                .max_steps(10_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let budget = asupersync::Budget::new().with_deadline(asupersync::Time::from_millis(5));
        let (task_id, _handle) = runtime
            .state
            .create_task(region, budget, async move {
                let result = timeout(Duration::from_secs(30), std::future::pending::<()>()).await;
                timed_out_task.store(result.is_err(), std::sync::atomic::Ordering::SeqCst);
            })
            .expect("spawn timeout task");
        runtime.scheduler.lock().schedule(task_id, 0);

        runtime.step_for_test();
        let virtual_time = runtime.run_with_auto_advance();
        let report = runtime.run_until_quiescent_with_report();

        assert!(
            virtual_time.auto_advances >= 1,
            "budget-bound timeout should advance to the tighter deadline"
        );
        assert!(
            runtime.now() >= asupersync::Time::from_millis(5),
            "virtual time should stop at or after the budget deadline"
        );
        assert!(
            timed_out.load(std::sync::atomic::Ordering::SeqCst),
            "timeout should report elapsed once the budget deadline is reached"
        );
        assert!(report.oracle_report.all_passed());
        assert!(report.invariant_violations.is_empty());
    }

    fn arb_labruntime_timer_deadlines_ms() -> impl Strategy<Value = Vec<u64>> {
        prop::collection::vec(0_u64..=50, 1..10)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn proptest_budget_meet_selects_earliest_deadline(
            left_ms in 0_u64..=1_000,
            right_ms in 0_u64..=1_000,
        ) {
            let left = asupersync::Budget::new().with_deadline(asupersync::Time::from_millis(left_ms));
            let right = asupersync::Budget::new().with_deadline(asupersync::Time::from_millis(right_ms));
            let combined = left.meet(right);
            let expected = asupersync::Time::from_millis(left_ms.min(right_ms));

            prop_assert_eq!(combined.deadline, Some(expected));
        }

        #[test]
        fn proptest_labruntime_timers_fire_in_deadline_order(
            deadlines_ms in arb_labruntime_timer_deadlines_ms(),
        ) {
            let fired = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(23)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);

            for &deadline_ms in &deadlines_ms {
                let fired_task = std::sync::Arc::clone(&fired);
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, asupersync::Budget::INFINITE, async move {
                        let cx = asupersync::Cx::current()
                            .expect("LabRuntime tasks should expose the current Cx");
                        sleep_with_cx(&cx, Duration::from_millis(deadline_ms))
                            .await
                            .expect("infinite-budget LabRuntime sleep should complete");
                        fired_task.lock().expect("timer order lock").push(deadline_ms);
                    })
                    .expect("spawn timer task");
                runtime.scheduler.lock().schedule(task_id, 0);
            }

            runtime.step_for_test();
            let report = runtime.run_with_auto_advance();
            let oracle_report = runtime.run_until_quiescent_with_report();
            let observed = fired.lock().expect("timer order lock").clone();
            let mut expected = deadlines_ms.clone();
            expected.sort_unstable();

            prop_assert_eq!(&observed, &expected);
            prop_assert_eq!(observed.len(), deadlines_ms.len());
            prop_assert!(
                report.auto_advances >= 1 || deadlines_ms.iter().all(|deadline| *deadline == 0),
                "non-zero timers should trigger at least one auto-advance"
            );
            prop_assert!(oracle_report.oracle_report.all_passed());
            prop_assert!(oracle_report.invariant_violations.is_empty());
        }
    }

    #[test]
    fn block_on_with_async_value() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let value = rt.block_on(async {
            let a = 10;
            let b = 20;
            a + b
        });
        assert_eq!(value, 30);
    }

    #[test]
    fn multi_thread_runtime_block_on() {
        let rt = RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let value = rt.block_on(async { "hello" });
        assert_eq!(value, "hello");
    }

    // ========================================================================
    // Mutex tests
    // ========================================================================

    #[test]
    fn mutex_lock_and_read() {
        run_async_test(async {
            let m = Mutex::new(42);
            let guard = m.lock().await;
            assert_eq!(*guard, 42);
        });
    }

    #[test]
    fn mutex_lock_and_mutate() {
        run_async_test(async {
            let m = Mutex::new(0);
            {
                let mut guard = m.lock().await;
                *guard = 99;
            }
            let guard = m.lock().await;
            assert_eq!(*guard, 99);
        });
    }

    #[test]
    fn mutex_sequential_locks() {
        run_async_test(async {
            let m = Mutex::new(vec![1, 2, 3]);
            {
                let mut guard = m.lock().await;
                guard.push(4);
            }
            let guard = m.lock().await;
            assert_eq!(*guard, vec![1, 2, 3, 4]);
        });
    }

    // ========================================================================
    // RwLock tests
    // ========================================================================

    #[test]
    fn rwlock_read() {
        run_async_test(async {
            let rw = RwLock::new("hello".to_string());
            let guard = rw.read().await;
            assert_eq!(&*guard, "hello");
        });
    }

    #[test]
    fn rwlock_write() {
        run_async_test(async {
            let rw = RwLock::new(0);
            {
                let mut guard = rw.write().await;
                *guard = 42;
            }
            let guard = rw.read().await;
            assert_eq!(*guard, 42);
        });
    }

    #[test]
    fn rwlock_multiple_sequential_readers() {
        run_async_test(async {
            let rw = RwLock::new(100);
            let r1 = rw.read().await;
            assert_eq!(*r1, 100);
            drop(r1);
            let r2 = rw.read().await;
            assert_eq!(*r2, 100);
        });
    }

    // ========================================================================
    // Semaphore tests
    // ========================================================================

    #[test]
    fn semaphore_available_permits() {
        run_async_test(async {
            let sem = Semaphore::new(3);
            assert_eq!(sem.available_permits(), 3);
        });
    }

    #[test]
    fn semaphore_acquire_decrements_permits() {
        run_async_test(async {
            let sem = Semaphore::new(2);
            let _p1 = sem.acquire().await.expect("acquire 1");
            assert_eq!(sem.available_permits(), 1);
        });
    }

    #[test]
    fn semaphore_release_on_drop() {
        run_async_test(async {
            let sem = Semaphore::new(1);
            {
                let _p = sem.acquire().await.expect("acquire");
                assert_eq!(sem.available_permits(), 0);
            }
            assert_eq!(sem.available_permits(), 1);
        });
    }

    #[test]
    fn semaphore_try_acquire_success() {
        run_async_test(async {
            let sem = Semaphore::new(1);
            let p = sem.try_acquire();
            assert!(p.is_ok());
        });
    }

    #[test]
    fn semaphore_try_acquire_no_permits() {
        run_async_test(async {
            let sem = Semaphore::new(1);
            let _held = sem.acquire().await.expect("acquire");
            let err = sem.try_acquire();
            assert!(err.is_err());
        });
    }

    #[test]
    fn semaphore_try_acquire_owned_success() {
        run_async_test(async {
            let sem = std::sync::Arc::new(Semaphore::new(2));
            let p = sem.clone().try_acquire_owned();
            assert!(p.is_ok());
        });
    }

    #[test]
    fn semaphore_try_acquire_owned_no_permits() {
        run_async_test(async {
            let sem = std::sync::Arc::new(Semaphore::new(1));
            let _held = sem.clone().acquire_owned().await.expect("acquire");
            let err = sem.clone().try_acquire_owned();
            assert!(err.is_err());
        });
    }

    // ========================================================================
    // MPSC channel tests
    // ========================================================================

    #[test]
    fn mpsc_send_recv() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel(10);
            {
                let cx = asupersync::Cx::for_testing();
                tx.send(&cx, 42).await.expect("send");
                let val = rx.recv(&cx).await.expect("recv");
                assert_eq!(val, 42);
            }
        });
    }

    #[test]
    fn mpsc_multiple_messages_fifo() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel(10);
            {
                let cx = asupersync::Cx::for_testing();
                for i in 0..5 {
                    tx.send(&cx, i).await.expect("send");
                }
            }
            for i in 0..5 {
                {
                    let cx = asupersync::Cx::for_testing();
                    let val = rx.recv(&cx).await.expect("recv");
                    assert_eq!(val, i);
                }
            }
        });
    }

    #[test]
    fn mpsc_send_and_recv_option_helpers_roundtrip() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel(4);
            mpsc_send(&tx, 7).await.expect("send helper");
            let got = mpsc_recv_option(&mut rx).await;
            assert_eq!(got, Some(7));
        });
    }

    #[test]
    fn mpsc_recv_option_helper_returns_none_when_closed() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            drop(tx);
            let got = mpsc_recv_option(&mut rx).await;
            assert_eq!(got, None);
        });
    }

    #[test]
    fn compat_channel_bridge_mpsc_try_send_error_maps_full_variant() {
        let err = mpsc::TrySendError::from(mpsc::SendError::Full("full"));
        match err {
            mpsc::TrySendError::Full(value) => assert_eq!(value, "full"),
            mpsc::TrySendError::Closed(value) => {
                panic!("expected Full variant, got Closed({value})")
            }
        }
    }

    #[test]
    fn compat_channel_bridge_mpsc_try_send_error_maps_closed_equivalents() {
        let disconnected = mpsc::TrySendError::from(mpsc::SendError::Disconnected("gone"));
        match disconnected {
            mpsc::TrySendError::Closed(value) => assert_eq!(value, "gone"),
            mpsc::TrySendError::Full(value) => {
                panic!("expected Closed variant for disconnected sender, got Full({value})")
            }
        }

        let cancelled = mpsc::TrySendError::from(mpsc::SendError::Cancelled("cancelled"));
        match cancelled {
            mpsc::TrySendError::Closed(value) => assert_eq!(value, "cancelled"),
            mpsc::TrySendError::Full(value) => {
                panic!("expected Closed variant for cancelled sender, got Full({value})")
            }
        }
    }

    // ========================================================================
    // Watch channel tests
    // ========================================================================

    #[test]
    fn watch_initial_value() {
        run_async_test(async {
            let (_, rx) = watch::channel(42);
            assert_eq!(*rx.borrow(), 42);
        });
    }

    #[test]
    fn watch_send_updates_value() {
        run_async_test(async {
            let (tx, rx) = watch::channel(0);
            tx.send(99).expect("send");
            assert_eq!(*rx.borrow(), 99);
        });
    }

    #[test]
    fn watch_has_changed_detects_new_value() {
        run_async_test(async {
            let (tx, mut rx) = watch::channel(0u32);
            assert!(!watch_has_changed(&rx));
            tx.send(5).expect("send");
            assert!(watch_has_changed(&rx));
            let latest = watch_borrow_and_update_clone(&mut rx);
            assert_eq!(latest, 5);
        });
    }

    #[test]
    fn watch_has_changed_handles_closed_channel() {
        run_async_test(async {
            let (tx, rx) = watch::channel(42u32);
            drop(tx);
            assert!(!watch_has_changed(&rx));
        });
    }

    #[test]
    fn watch_borrow_and_update_clone_returns_latest_value() {
        run_async_test(async {
            let (tx, mut rx) = watch::channel(vec![1u8, 2u8]);
            tx.send(vec![3u8, 4u8]).expect("send");
            let latest = watch_borrow_and_update_clone(&mut rx);
            assert_eq!(latest, vec![3u8, 4u8]);
        });
    }

    #[test]
    fn compat_channel_bridge_watch_changed_observes_update() {
        run_async_test(async {
            let (tx, mut rx) = watch::channel(0u32);

            task::spawn(async move {
                sleep(Duration::from_millis(5)).await;
                tx.send(9).expect("send update");
            });

            watch_changed(&mut rx)
                .await
                .expect("watch_changed should observe the update");
            assert_eq!(watch_borrow_and_update_clone(&mut rx), 9);
            assert!(
                !watch_has_changed(&rx),
                "consuming the update should clear the changed flag"
            );
        });
    }

    #[test]
    fn compat_channel_bridge_watch_changed_returns_error_when_closed() {
        run_async_test(async {
            let (tx, mut rx) = watch::channel(41u32);
            drop(tx);

            let err = watch_changed(&mut rx)
                .await
                .expect_err("watch_changed should report closure when no further updates exist");
            let display = err.to_string();
            assert!(
                !display.is_empty(),
                "watch_changed closure error should have a meaningful display form"
            );
        });
    }

    // ========================================================================
    // Broadcast channel tests
    // ========================================================================

    #[test]
    fn broadcast_send_recv() {
        run_async_test(async {
            let (tx, mut rx) = broadcast::channel(16);
            broadcast_send(&tx, 42).expect("send");
            let val = broadcast_recv(&mut rx).await.expect("recv");
            assert_eq!(val, 42);
        });
    }

    #[test]
    fn broadcast_multiple_receivers() {
        run_async_test(async {
            let (tx, mut rx1) = broadcast::channel(16);
            let mut rx2 = tx.subscribe();
            broadcast_send(&tx, 7).expect("send");
            assert_eq!(broadcast_recv(&mut rx1).await.expect("r1"), 7);
            assert_eq!(broadcast_recv(&mut rx2).await.expect("r2"), 7);
        });
    }

    #[test]
    fn immediately_ready_durable_channel_polls_do_not_allocate_forwarding_boundaries() {
        let cx = asupersync::Cx::for_testing();
        let caller_waker = futures::task::noop_waker();
        let mut caller_cx = std::task::Context::from_waker(&caller_waker);

        let (reserve_tx, _reserve_rx) = mpsc::channel::<u8>(1);
        let mut reserve = Box::pin(reserve_tx.reserve(&cx));
        let permit = match reserve.as_mut().poll(&mut caller_cx) {
            std::task::Poll::Ready(Ok(permit)) => permit,
            other => panic!("uncontended reserve was not ready: {other:?}"),
        };
        assert!(
            !reserve
                .as_ref()
                .get_ref()
                .retained_waker_allocated_for_test()
        );
        permit.abort();

        let (watch_tx, mut watch_rx) = watch::channel(0u8);
        watch_tx.send(2).expect("publish immediate watch value");
        let mut changed = Box::pin(watch_rx.changed(&cx));
        assert!(matches!(
            changed.as_mut().poll(&mut caller_cx),
            std::task::Poll::Ready(Ok(()))
        ));
        drop(changed);
        assert!(!watch_rx.retained_waker_allocated_for_test());

        let (broadcast_tx, mut broadcast_rx) = broadcast::channel(1);
        broadcast_tx
            .send_with_cx(&cx, 3)
            .expect("publish immediate broadcast value");
        let mut broadcast_receive = Box::pin(broadcast_rx.recv_with_cx(&cx));
        assert!(matches!(
            broadcast_receive.as_mut().poll(&mut caller_cx),
            std::task::Poll::Ready(Ok(3))
        ));
        drop(broadcast_receive);
        assert!(!broadcast_rx.retained_waker_allocated_for_test());
    }

    #[test]
    fn contained_proxy_forwards_a_nondurable_first_poll_wake() {
        static LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        static CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);

        let mut retained_waker = None;
        let (caller_probe, caller_waker) = probe_waker(false);
        let caller_cx = std::task::Context::from_waker(&caller_waker);
        let mut inner_poll_count = 0usize;
        let result = poll_with_contained_channel_waker(
            &mut retained_waker,
            &caller_cx,
            |inner_cx| {
                inner_poll_count += 1;
                // Model `mpsc::Sender::wake_receiver()`: this wake carries no
                // durable message or closed-state transition for a later
                // probe to observe.
                inner_cx.waker().wake_by_ref();
                std::task::Poll::<()>::Pending
            },
            || ContainedWakerBoundary::new(&LOCK_POISONED_COUNT, &CALLBACK_PANIC_COUNT),
            || (),
        );

        assert!(result.is_pending());
        assert_eq!(inner_poll_count, 1);
        assert_eq!(
            caller_probe.count(),
            1,
            "an edge-triggered first-poll wake must reach the caller"
        );
        assert!(retained_waker.is_some());
    }

    #[test]
    fn immediately_ready_mpsc_receive_uses_the_edge_safe_boundary() {
        let cx = asupersync::Cx::for_testing();
        let caller_waker = futures::task::noop_waker();
        let mut caller_cx = std::task::Context::from_waker(&caller_waker);
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(1).expect("queue immediate MPSC value");

        let mut receive = Box::pin(rx.recv(&cx));
        assert!(matches!(
            receive.as_mut().poll(&mut caller_cx),
            std::task::Poll::Ready(Ok(1))
        ));
        drop(receive);
        assert!(
            rx.retained_waker_allocated_for_test(),
            "MPSC's out-of-band wake_receiver contract requires containment before first poll"
        );
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn contained_channel_waker_pending_repoll_polls_inner_once() {
        static LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        static CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);

        let mut retained_waker = None;
        let (drop_count, drop_panicking) = drop_panicking_waker();
        let mut initial_poll_count = 0usize;
        {
            let hostile_cx = std::task::Context::from_waker(&drop_panicking);
            let initial = poll_with_durable_probe_contained_channel_waker(
                &mut retained_waker,
                &hostile_cx,
                |_| {
                    initial_poll_count += 1;
                    std::task::Poll::<()>::Pending
                },
                || ContainedWakerBoundary::new(&LOCK_POISONED_COUNT, &CALLBACK_PANIC_COUNT),
                || (),
            );
            assert!(initial.is_pending());
        }
        assert_eq!(
            initial_poll_count, 2,
            "the boundary-free probe must repoll once to publish its proxy"
        );
        assert!(retained_waker.is_some());

        drop(drop_panicking);
        let (_, replacement_waker) = probe_waker(false);
        let replacement_cx = std::task::Context::from_waker(&replacement_waker);
        let mut repoll_count = 0usize;
        let repoll = poll_with_durable_probe_contained_channel_waker(
            &mut retained_waker,
            &replacement_cx,
            |_| {
                repoll_count += 1;
                std::task::Poll::<()>::Pending
            },
            || ContainedWakerBoundary::new(&LOCK_POISONED_COUNT, &CALLBACK_PANIC_COUNT),
            || (),
        );
        assert!(repoll.is_pending());
        assert_eq!(
            repoll_count, 1,
            "an existing proxy must not double-poll a sustained pending operation"
        );
        assert_eq!(
            drop_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "repoll replacement must quarantine the hostile caller-waker destructor"
        );
        assert_eq!(
            CALLBACK_PANIC_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        retained_waker
            .as_ref()
            .expect("pending repoll must retain its stable proxy")
            .clear();
    }

    #[test]
    fn contained_channel_waker_second_poll_closes_noop_probe_race() {
        static LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        static CALLBACK_PANIC_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);

        let mut retained_waker = None;
        let (caller_probe, caller_waker) = probe_waker(false);
        let caller_cx = std::task::Context::from_waker(&caller_waker);
        let mut inner_poll_count = 0usize;
        let result = poll_with_durable_probe_contained_channel_waker(
            &mut retained_waker,
            &caller_cx,
            |inner_cx| {
                inner_poll_count += 1;
                if inner_poll_count == 1 {
                    inner_cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(41u8)
                }
            },
            || ContainedWakerBoundary::new(&LOCK_POISONED_COUNT, &CALLBACK_PANIC_COUNT),
            || 0,
        );
        assert_eq!(result, std::task::Poll::Ready(41));
        assert_eq!(inner_poll_count, 2);
        assert_eq!(
            caller_probe.count(),
            0,
            "the wrapper itself consumes a first-probe wake by immediately repolling"
        );

        let boundary = retained_waker
            .as_ref()
            .expect("a pending first probe must allocate its stable proxy");
        boundary.proxy().wake_by_ref();
        assert_eq!(
            caller_probe.count(),
            0,
            "a race-closing Ready result must clear the caller registration"
        );
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn mpsc_wait_surfaces_contain_hostile_wakes_and_repoll_replacement() {
        let cx = asupersync::Cx::for_testing();

        let (tx, mut rx) = mpsc::channel(2);
        let mut receive = Box::pin(rx.recv(&cx));
        let (drop_count, drop_panicking) = drop_panicking_waker();
        {
            let mut first_cx = std::task::Context::from_waker(&drop_panicking);
            assert!(receive.as_mut().poll(&mut first_cx).is_pending());
        }
        drop(drop_panicking);

        let (replacement_probe, replacement_waker) = probe_waker(false);
        let mut replacement_cx = std::task::Context::from_waker(&replacement_waker);
        assert!(receive.as_mut().poll(&mut replacement_cx).is_pending());
        assert_eq!(
            drop_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "repoll replacement must dispose the retired caller waker"
        );
        tx.try_send(11).expect("wake contained receiver");
        assert_eq!(replacement_probe.count(), 1);
        assert!(matches!(
            receive.as_mut().poll(&mut replacement_cx),
            std::task::Poll::Ready(Ok(11))
        ));
        drop(receive);

        let callback_before = mpsc::retained_waker_callback_panic_count();
        let mut batch = Vec::new();
        let mut receive_many = Box::pin(rx.recv_many(&cx, &mut batch, 8));
        let (hostile_batch_probe, hostile_batch_waker) = probe_waker(true);
        let mut hostile_batch_cx = std::task::Context::from_waker(&hostile_batch_waker);
        assert!(
            receive_many
                .as_mut()
                .poll(&mut hostile_batch_cx)
                .is_pending()
        );
        tx.try_send(12).expect("wake contained batch receiver");
        assert_eq!(hostile_batch_probe.count(), 1);
        assert!(mpsc::retained_waker_callback_panic_count() >= callback_before.saturating_add(1));
        let (_, normal_batch_waker) = probe_waker(false);
        let mut normal_batch_cx = std::task::Context::from_waker(&normal_batch_waker);
        assert!(matches!(
            receive_many.as_mut().poll(&mut normal_batch_cx),
            std::task::Poll::Ready(Ok(1))
        ));
        drop(receive_many);
        assert_eq!(batch, vec![12]);

        let callback_before = mpsc::retained_waker_callback_panic_count();
        let (hostile_poll_probe, hostile_poll_waker) = probe_waker(true);
        let mut hostile_poll_cx = std::task::Context::from_waker(&hostile_poll_waker);
        assert!(rx.poll_recv(&cx, &mut hostile_poll_cx).is_pending());
        tx.try_send(13)
            .expect("wake contained direct poll receiver");
        assert_eq!(hostile_poll_probe.count(), 1);
        assert!(mpsc::retained_waker_callback_panic_count() >= callback_before.saturating_add(1));
        let (_, normal_poll_waker) = probe_waker(false);
        let mut normal_poll_cx = std::task::Context::from_waker(&normal_poll_waker);
        assert!(matches!(
            rx.poll_recv(&cx, &mut normal_poll_cx),
            std::task::Poll::Ready(Ok(13))
        ));

        let mut direct_batch = Vec::new();
        let (hostile_many_probe, hostile_many_waker) = probe_waker(true);
        let mut hostile_many_cx = std::task::Context::from_waker(&hostile_many_waker);
        assert!(
            rx.poll_recv_many(&cx, &mut direct_batch, 4, &mut hostile_many_cx)
                .is_pending()
        );
        tx.try_send(14)
            .expect("wake contained direct batch receiver");
        assert_eq!(hostile_many_probe.count(), 1);
        let (_, normal_many_waker) = probe_waker(false);
        let mut normal_many_cx = std::task::Context::from_waker(&normal_many_waker);
        assert!(matches!(
            rx.poll_recv_many(&cx, &mut direct_batch, 4, &mut normal_many_cx),
            std::task::Poll::Ready(Ok(1))
        ));
        assert_eq!(direct_batch, vec![14]);
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn mpsc_capacity_and_close_wakes_do_not_strand_later_waiters() {
        let cx = asupersync::Cx::for_testing();

        let (capacity_tx, mut capacity_rx) = mpsc::channel(1);
        capacity_tx.try_send(0).expect("fill capacity channel");
        let mut capacity_first = Box::pin(capacity_tx.reserve(&cx));
        let mut capacity_second = Box::pin(capacity_tx.reserve(&cx));
        let (capacity_hostile_probe, capacity_hostile_waker) = probe_waker(true);
        let (capacity_later_probe, capacity_later_waker) = probe_waker(false);
        let mut capacity_hostile_cx = std::task::Context::from_waker(&capacity_hostile_waker);
        let mut capacity_later_cx = std::task::Context::from_waker(&capacity_later_waker);
        assert!(
            capacity_first
                .as_mut()
                .poll(&mut capacity_hostile_cx)
                .is_pending()
        );
        assert!(
            capacity_second
                .as_mut()
                .poll(&mut capacity_later_cx)
                .is_pending()
        );
        assert_eq!(capacity_rx.try_recv(), Ok(0));
        assert_eq!(capacity_hostile_probe.count(), 1);
        let (_, capacity_repoll_waker) = probe_waker(false);
        let mut capacity_repoll_cx = std::task::Context::from_waker(&capacity_repoll_waker);
        let first_permit = match capacity_first.as_mut().poll(&mut capacity_repoll_cx) {
            std::task::Poll::Ready(Ok(permit)) => permit,
            other => panic!("head capacity waiter did not acquire: {other:?}"),
        };
        first_permit.abort();
        assert_eq!(
            capacity_later_probe.count(),
            1,
            "releasing the first reservation must wake the later waiter"
        );
        let second_permit = match capacity_second.as_mut().poll(&mut capacity_later_cx) {
            std::task::Poll::Ready(Ok(permit)) => permit,
            other => panic!("later capacity waiter was stranded: {other:?}"),
        };
        second_permit.abort();

        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(1).expect("fill channel");

        let mut first = Box::pin(tx.reserve(&cx));
        let mut second = Box::pin(tx.reserve(&cx));
        let (hostile_probe, hostile_waker) = probe_waker(true);
        let (normal_probe, normal_waker) = probe_waker(false);
        let mut hostile_cx = std::task::Context::from_waker(&hostile_waker);
        let mut normal_cx = std::task::Context::from_waker(&normal_waker);
        assert!(first.as_mut().poll(&mut hostile_cx).is_pending());
        assert!(second.as_mut().poll(&mut normal_cx).is_pending());

        let callback_before = mpsc::retained_waker_callback_panic_count();
        let close_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rx.close()));
        assert!(
            close_result.is_ok(),
            "close must contain every waiter callback"
        );
        assert_eq!(hostile_probe.count(), 1);
        assert_eq!(normal_probe.count(), 1, "later waiter must still be woken");
        assert!(mpsc::retained_waker_callback_panic_count() >= callback_before.saturating_add(1));
        assert!(matches!(
            first.as_mut().poll(&mut hostile_cx),
            std::task::Poll::Ready(Err(mpsc::SendError::Disconnected(())))
        ));
        assert!(matches!(
            second.as_mut().poll(&mut normal_cx),
            std::task::Poll::Ready(Err(mpsc::SendError::Disconnected(())))
        ));
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn watch_and_broadcast_multi_receiver_wake_loops_survive_hostile_waiters() {
        let cx = asupersync::Cx::for_testing();

        let (watch_tx, mut watch_rx1) = watch::channel(0u8);
        let mut watch_rx2 = watch_tx.subscribe();
        let mut watch_first = Box::pin(watch_rx1.changed(&cx));
        let mut watch_second = Box::pin(watch_rx2.changed(&cx));
        let (watch_hostile_probe, watch_hostile_waker) = probe_waker(true);
        let (watch_normal_probe, watch_normal_waker) = probe_waker(false);
        let mut watch_hostile_cx = std::task::Context::from_waker(&watch_hostile_waker);
        let mut watch_normal_cx = std::task::Context::from_waker(&watch_normal_waker);
        assert!(
            watch_first
                .as_mut()
                .poll(&mut watch_hostile_cx)
                .is_pending()
        );
        assert!(
            watch_second
                .as_mut()
                .poll(&mut watch_normal_cx)
                .is_pending()
        );
        let watch_before = watch::retained_waker_callback_panic_count();
        let watch_send =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| watch_tx.send(1)));
        assert!(matches!(watch_send, Ok(Ok(()))));
        assert_eq!(watch_hostile_probe.count(), 1);
        assert_eq!(watch_normal_probe.count(), 1);
        assert!(watch::retained_waker_callback_panic_count() >= watch_before.saturating_add(1));
        assert!(watch_first.as_mut().poll(&mut watch_hostile_cx).is_ready());
        assert!(watch_second.as_mut().poll(&mut watch_normal_cx).is_ready());

        let (broadcast_tx, mut broadcast_rx1) = broadcast::channel(4);
        let mut broadcast_rx2 = broadcast_tx.subscribe();
        let mut broadcast_first = Box::pin(broadcast_rx1.recv_with_cx(&cx));
        let mut broadcast_second = Box::pin(broadcast_rx2.recv_with_cx(&cx));
        let (broadcast_hostile_probe, broadcast_hostile_waker) = probe_waker(true);
        let (broadcast_normal_probe, broadcast_normal_waker) = probe_waker(false);
        let mut broadcast_hostile_cx = std::task::Context::from_waker(&broadcast_hostile_waker);
        let mut broadcast_normal_cx = std::task::Context::from_waker(&broadcast_normal_waker);
        assert!(
            broadcast_first
                .as_mut()
                .poll(&mut broadcast_hostile_cx)
                .is_pending()
        );
        assert!(
            broadcast_second
                .as_mut()
                .poll(&mut broadcast_normal_cx)
                .is_pending()
        );
        let broadcast_before = broadcast::retained_waker_callback_panic_count();
        let broadcast_send = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            broadcast_tx.send_with_cx(&cx, 2)
        }));
        assert!(matches!(broadcast_send, Ok(Ok(2))));
        assert_eq!(broadcast_hostile_probe.count(), 1);
        assert_eq!(broadcast_normal_probe.count(), 1);
        assert!(
            broadcast::retained_waker_callback_panic_count() >= broadcast_before.saturating_add(1)
        );
        assert!(matches!(
            broadcast_first.as_mut().poll(&mut broadcast_hostile_cx),
            std::task::Poll::Ready(Ok(2))
        ));
        assert!(matches!(
            broadcast_second.as_mut().poll(&mut broadcast_normal_cx),
            std::task::Poll::Ready(Ok(2))
        ));
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn pending_channel_future_drops_allow_reentrant_waker_destructors() {
        let cx = asupersync::Cx::for_testing();

        let (mpsc_tx, mut mpsc_rx) = mpsc::channel(1);
        let mpsc_completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mpsc_waker = std::task::Waker::from(std::sync::Arc::new(MpscReentrantDropWake {
            sender: mpsc_tx,
            completed: std::sync::Arc::clone(&mpsc_completed),
        }));
        let mut mpsc_future = Box::pin(mpsc_rx.recv(&cx));
        {
            let mut receive_poll_context = std::task::Context::from_waker(&mpsc_waker);
            assert!(
                mpsc_future
                    .as_mut()
                    .poll(&mut receive_poll_context)
                    .is_pending()
            );
        }
        drop(mpsc_waker);
        drop(mpsc_future);
        assert!(mpsc_completed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(mpsc_rx.try_recv(), Ok(73));

        let (watch_tx, mut watch_rx) = watch::channel(0u8);
        let watch_completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watch_waker = std::task::Waker::from(std::sync::Arc::new(WatchReentrantDropWake {
            sender: watch_tx,
            completed: std::sync::Arc::clone(&watch_completed),
        }));
        let mut watch_future = Box::pin(watch_rx.changed(&cx));
        {
            let mut change_poll_context = std::task::Context::from_waker(&watch_waker);
            assert!(
                watch_future
                    .as_mut()
                    .poll(&mut change_poll_context)
                    .is_pending()
            );
        }
        drop(watch_waker);
        drop(watch_future);
        assert!(watch_completed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(*watch_rx.borrow(), 83);

        let (broadcast_tx, mut broadcast_rx) = broadcast::channel(2);
        let broadcast_completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let broadcast_waker =
            std::task::Waker::from(std::sync::Arc::new(BroadcastReentrantDropWake {
                sender: broadcast_tx,
                completed: std::sync::Arc::clone(&broadcast_completed),
            }));
        let mut broadcast_future = Box::pin(broadcast_rx.recv_with_cx(&cx));
        {
            let mut fanout_poll_context = std::task::Context::from_waker(&broadcast_waker);
            assert!(
                broadcast_future
                    .as_mut()
                    .poll(&mut fanout_poll_context)
                    .is_pending()
            );
        }
        drop(broadcast_waker);
        drop(broadcast_future);
        assert!(broadcast_completed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(broadcast_rx.try_recv(), Ok(97));
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn pending_channel_future_drops_contain_hostile_waker_destructors() {
        let cx = asupersync::Cx::for_testing();

        let (reserve_tx, mut reserve_rx) = mpsc::channel::<u8>(1);
        reserve_tx.try_send(1).expect("fill reserve channel");
        let (reserve_drop_count, reserve_waker) = drop_panicking_waker();
        let mut reserve_future = Box::pin(reserve_tx.reserve(&cx));
        {
            let mut permit_poll_context = std::task::Context::from_waker(&reserve_waker);
            assert!(
                reserve_future
                    .as_mut()
                    .poll(&mut permit_poll_context)
                    .is_pending()
            );
        }
        drop(reserve_waker);
        let reserve_before = mpsc::retained_waker_callback_panic_count();
        drop(reserve_future);
        assert_eq!(
            reserve_drop_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(mpsc::retained_waker_callback_panic_count() >= reserve_before.saturating_add(1));
        assert_eq!(reserve_rx.try_recv(), Ok(1));

        let (_mpsc_tx, mut mpsc_rx) = mpsc::channel::<u8>(1);
        let (mpsc_drop_count, mpsc_waker) = drop_panicking_waker();
        let mut mpsc_future = Box::pin(mpsc_rx.recv(&cx));
        {
            let mut receive_poll_context = std::task::Context::from_waker(&mpsc_waker);
            assert!(
                mpsc_future
                    .as_mut()
                    .poll(&mut receive_poll_context)
                    .is_pending()
            );
        }
        drop(mpsc_waker);
        let mpsc_before = mpsc::retained_waker_callback_panic_count();
        drop(mpsc_future);
        assert_eq!(mpsc_drop_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(mpsc::retained_waker_callback_panic_count() >= mpsc_before.saturating_add(1));

        let (_watch_tx, mut watch_rx) = watch::channel(0u8);
        let (watch_drop_count, watch_waker) = drop_panicking_waker();
        let mut watch_future = Box::pin(watch_rx.changed(&cx));
        {
            let mut change_poll_context = std::task::Context::from_waker(&watch_waker);
            assert!(
                watch_future
                    .as_mut()
                    .poll(&mut change_poll_context)
                    .is_pending()
            );
        }
        drop(watch_waker);
        let watch_before = watch::retained_waker_callback_panic_count();
        drop(watch_future);
        assert_eq!(
            watch_drop_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(watch::retained_waker_callback_panic_count() >= watch_before.saturating_add(1));

        let (_broadcast_tx, mut broadcast_rx) = broadcast::channel::<u8>(1);
        let (broadcast_drop_count, broadcast_waker) = drop_panicking_waker();
        let mut broadcast_future = Box::pin(broadcast_rx.recv_with_cx(&cx));
        {
            let mut fanout_poll_context = std::task::Context::from_waker(&broadcast_waker);
            assert!(
                broadcast_future
                    .as_mut()
                    .poll(&mut fanout_poll_context)
                    .is_pending()
            );
        }
        drop(broadcast_waker);
        let broadcast_before = broadcast::retained_waker_callback_panic_count();
        drop(broadcast_future);
        assert_eq!(
            broadcast_drop_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            broadcast::retained_waker_callback_panic_count() >= broadcast_before.saturating_add(1)
        );
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn pending_mpsc_drop_contains_nested_waker_panic_during_outer_unwind() {
        let cx = asupersync::Cx::for_testing();
        let (tx, mut rx) = mpsc::channel::<u8>(1);
        let (drop_count, drop_panicking) = drop_panicking_waker();
        let outer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut receive = Box::pin(rx.recv(&cx));
            {
                let mut caller_cx = std::task::Context::from_waker(&drop_panicking);
                assert!(receive.as_mut().poll(&mut caller_cx).is_pending());
            }
            drop(drop_panicking);
            let _keep_sender_live = tx;
            panic!("synthetic authoritative outer panic");
        }));
        assert!(
            outer.is_err(),
            "the authoritative outer panic must remain visible"
        );
        assert_eq!(
            drop_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "nested caller-waker destructor panic must be contained exactly once"
        );
    }

    // ========================================================================
    // Sleep and timeout edge cases
    // ========================================================================

    #[test]
    fn sleep_zero_duration_completes_immediately() {
        run_async_test(async {
            let start = std::time::Instant::now();
            sleep(Duration::ZERO).await;
            assert!(start.elapsed() < Duration::from_millis(100));
        });
    }

    #[test]
    fn timeout_with_immediate_future() {
        run_async_test(async {
            let result = timeout(Duration::from_millis(100), async { "fast" }).await;
            assert_eq!(result.unwrap(), "fast");
        });
    }

    #[test]
    fn timeout_error_is_string() {
        run_async_test(async {
            let result = timeout(Duration::from_millis(1), async {
                sleep(Duration::from_secs(10)).await;
            })
            .await;
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(!err.is_empty());
        });
    }

    // ========================================================================
    // CompatRuntime trait tests
    // ========================================================================

    #[test]
    fn block_on_returns_complex_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Vec<i32> = rt.block_on(async { vec![1, 2, 3] });
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[test]
    fn spawn_detached_accepts_send_future() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {});
        rt.spawn_detached(async {});
    }

    // ========================================================================
    // NEW TESTS: RuntimeBuilder edge cases
    // ========================================================================

    #[test]
    fn runtime_builder_worker_threads_one() {
        // Minimum meaningful worker thread count
        let rt = RuntimeBuilder::multi_thread().worker_threads(1).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_multi_thread_without_worker_threads_uses_default() {
        // multi_thread without explicit worker_threads should use system default
        let rt = RuntimeBuilder::multi_thread().build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_current_thread_ignores_worker_threads_one() {
        // current_thread silently ignores worker_threads(1)
        let rt = RuntimeBuilder::current_thread().worker_threads(1).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_current_thread_worker_threads_large() {
        // current_thread should silently ignore even large worker_threads values
        let rt = RuntimeBuilder::current_thread().worker_threads(128).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_build_returns_result() {
        // Verify the build() return type is Result<Runtime, String>
        let result: Result<Runtime, String> = RuntimeBuilder::current_thread().build();
        assert!(result.is_ok());
    }

    // ========================================================================
    // NEW TESTS: CompatRuntime block_on edge cases
    // ========================================================================

    #[test]
    fn block_on_returns_unit() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: () = rt.block_on(async {});
        assert_eq!(result, ());
    }

    #[test]
    fn block_on_returns_result_ok() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Result<i32, String> = rt.block_on(async { Ok(42) });
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn block_on_returns_result_err() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Result<i32, String> = rt.block_on(async { Err("oops".to_string()) });
        assert_eq!(result.unwrap_err(), "oops");
    }

    #[test]
    fn block_on_returns_option_some() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Option<u64> = rt.block_on(async { Some(100) });
        assert_eq!(result, Some(100));
    }

    #[test]
    fn block_on_returns_option_none() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Option<u64> = rt.block_on(async { None });
        assert_eq!(result, None);
    }

    #[test]
    fn block_on_with_string_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async { String::from("async string") });
        assert_eq!(result, "async string");
    }

    #[test]
    fn block_on_with_nested_async_computation() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async {
            let a = async { 10 }.await;
            let b = async { 20 }.await;
            a + b
        });
        assert_eq!(result, 30);
    }

    #[test]
    fn multi_thread_block_on_returns_tuple() {
        let rt = RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let (a, b) = rt.block_on(async { (1, "two") });
        assert_eq!(a, 1);
        assert_eq!(b, "two");
    }

    #[test]
    fn spawn_detached_from_multi_thread_runtime() {
        let rt = RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        // Should not panic even from multi-threaded runtime
        rt.spawn_detached(async {});
    }

    // ========================================================================
    // NEW TESTS: Mutex edge cases
    // ========================================================================

    #[test]
    fn mutex_with_string_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let m = Mutex::new(String::from("initial"));
            {
                let mut guard = m.lock().await;
                guard.push_str(" modified");
            }
            let guard = m.lock().await;
            assert_eq!(&*guard, "initial modified");
        });
    }

    #[test]
    fn mutex_with_hashmap() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            use std::collections::HashMap;
            let m = Mutex::new(HashMap::new());
            {
                let mut guard = m.lock().await;
                guard.insert("key", 42);
            }
            let guard = m.lock().await;
            assert_eq!(guard.get("key"), Some(&42));
        });
    }

    #[test]
    fn mutex_with_option_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let m = Mutex::new(None::<u32>);
            {
                let mut guard = m.lock().await;
                *guard = Some(7);
            }
            let guard = m.lock().await;
            assert_eq!(*guard, Some(7));
        });
    }

    #[test]
    fn mutex_multiple_lock_unlock_cycles() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let m = Mutex::new(0u64);
            for i in 0..10 {
                let mut guard = m.lock().await;
                *guard = i;
            }
            let guard = m.lock().await;
            assert_eq!(*guard, 9);
        });
    }

    #[test]
    fn mutex_deref_read_access() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let m = Mutex::new(vec![10, 20, 30]);
            let guard = m.lock().await;
            // Test Deref: can call Vec methods via guard
            assert_eq!(guard.len(), 3);
            assert!(guard.contains(&20));
        });
    }

    // ========================================================================
    // NEW TESTS: RwLock edge cases
    // ========================================================================

    #[test]
    fn rwlock_write_then_write() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let rw = RwLock::new(0);
            {
                let mut guard = rw.write().await;
                *guard = 10;
            }
            {
                let mut guard = rw.write().await;
                *guard += 5;
            }
            let guard = rw.read().await;
            assert_eq!(*guard, 15);
        });
    }

    #[test]
    fn rwlock_with_string_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let rw = RwLock::new(String::new());
            {
                let mut guard = rw.write().await;
                guard.push_str("hello");
            }
            let guard = rw.read().await;
            assert_eq!(&*guard, "hello");
        });
    }

    #[test]
    fn rwlock_with_vec_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let rw = RwLock::new(Vec::<i32>::new());
            {
                let mut guard = rw.write().await;
                guard.extend_from_slice(&[1, 2, 3]);
            }
            let guard = rw.read().await;
            assert_eq!(guard.len(), 3);
            assert_eq!(&*guard, &[1, 2, 3]);
        });
    }

    #[test]
    fn rwlock_read_does_not_mutate() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let rw = RwLock::new(42);
            {
                let guard = rw.read().await;
                assert_eq!(*guard, 42);
            }
            // Value unchanged
            let guard = rw.read().await;
            assert_eq!(*guard, 42);
        });
    }

    #[test]
    fn rwlock_multiple_write_cycles() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let rw = RwLock::new(0i64);
            for i in 0..5 {
                let mut guard = rw.write().await;
                *guard += i;
            }
            // Sum of 0..5 = 0+1+2+3+4 = 10
            let guard = rw.read().await;
            assert_eq!(*guard, 10);
        });
    }

    // ========================================================================
    // NEW TESTS: Semaphore edge cases
    // ========================================================================

    #[test]
    fn semaphore_zero_permits() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(0);
            assert_eq!(sem.available_permits(), 0);
            // try_acquire should fail immediately with zero permits
            let result = sem.try_acquire();
            assert!(result.is_err());
        });
    }

    #[test]
    fn semaphore_close_then_try_acquire() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(5);
            sem.close();
            let result = sem.try_acquire();
            assert!(result.is_err());
        });
    }

    #[test]
    fn semaphore_close_then_try_acquire_owned() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = std::sync::Arc::new(Semaphore::new(5));
            sem.close();
            let result = sem.clone().try_acquire_owned();
            assert!(result.is_err());
        });
    }

    #[test]
    fn semaphore_acquire_all_permits_then_release() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(3);
            let p1 = sem.acquire().await.expect("acquire 1");
            let p2 = sem.acquire().await.expect("acquire 2");
            let p3 = sem.acquire().await.expect("acquire 3");
            assert_eq!(sem.available_permits(), 0);

            drop(p1);
            assert_eq!(sem.available_permits(), 1);
            drop(p2);
            assert_eq!(sem.available_permits(), 2);
            drop(p3);
            assert_eq!(sem.available_permits(), 3);
        });
    }

    #[test]
    fn semaphore_large_permit_count() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(10000);
            assert_eq!(sem.available_permits(), 10000);
            let _p = sem.try_acquire().expect("should acquire from large pool");
            assert_eq!(sem.available_permits(), 9999);
        });
    }

    #[test]
    fn semaphore_owned_acquire_and_release() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = std::sync::Arc::new(Semaphore::new(2));
            let p1 = sem.clone().acquire_owned().await.expect("acquire 1");
            assert_eq!(sem.available_permits(), 1);
            let p2 = sem.clone().acquire_owned().await.expect("acquire 2");
            assert_eq!(sem.available_permits(), 0);
            drop(p1);
            assert_eq!(sem.available_permits(), 1);
            drop(p2);
            assert_eq!(sem.available_permits(), 2);
        });
    }

    #[test]
    fn semaphore_try_acquire_returns_permit_on_success() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(1);
            let permit = sem.try_acquire();
            assert!(permit.is_ok());
            assert_eq!(sem.available_permits(), 0);
            drop(permit);
            assert_eq!(sem.available_permits(), 1);
        });
    }

    #[test]
    fn semaphore_close_preserves_held_permits() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(2);
            let _p = sem.acquire().await.expect("acquire");
            assert_eq!(sem.available_permits(), 1);
            sem.close();
            // After close, available permits may still be reported
            // but new acquires should fail
            let result = sem.try_acquire();
            assert!(result.is_err());
        });
    }

    // ========================================================================
    // NEW TESTS: MPSC channel edge cases
    // ========================================================================

    #[test]
    fn mpsc_send_helper_to_closed_receiver_returns_error() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = mpsc::channel::<i32>(1);
            drop(rx);
            let result = mpsc_send(&tx, 42).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn mpsc_reserve_send_roundtrip() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(1);
            assert!(mpsc_reserve_send(&tx, 11).await);
            assert_eq!(mpsc_recv_option(&mut rx).await, Some(11));
        });
    }

    #[test]
    fn mpsc_reserve_send_returns_false_when_receiver_closed() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = mpsc::channel::<i32>(1);
            drop(rx);
            assert!(!mpsc_reserve_send(&tx, 7).await);
        });
    }

    #[test]
    fn mpsc_try_reserve_send_reports_full_queue() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(1);
            assert!(mpsc_try_reserve_send(&tx, 1));
            assert!(!mpsc_try_reserve_send(&tx, 2));
            assert_eq!(mpsc_recv_option(&mut rx).await, Some(1));
        });
    }

    #[test]
    fn mpsc_send_recv_string_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(4);
            mpsc_send(&tx, String::from("hello")).await.expect("send");
            let got = mpsc_recv_option(&mut rx).await;
            assert_eq!(got, Some(String::from("hello")));
        });
    }

    #[test]
    fn mpsc_multiple_messages_via_helpers() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(8);
            for i in 0..5u32 {
                mpsc_send(&tx, i).await.expect("send");
            }
            for i in 0..5u32 {
                let got = mpsc_recv_option(&mut rx).await;
                assert_eq!(got, Some(i));
            }
        });
    }

    #[test]
    fn mpsc_channel_capacity_one() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(1);
            mpsc_send(&tx, 99u8).await.expect("send");
            let got = mpsc_recv_option(&mut rx).await;
            assert_eq!(got, Some(99u8));
        });
    }

    #[test]
    fn mpsc_send_error_contains_value() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = mpsc::channel::<String>(1);
            drop(rx);
            let err = mpsc_send(&tx, String::from("lost")).await;
            assert!(err.is_err());
            // The SendError should contain the value that could not be sent
            let send_err = err.unwrap_err();
            assert!(
                matches!(
                    send_err,
                    mpsc::SendError::Disconnected(value) if value == "lost"
                ),
                "expected disconnected send error carrying original value",
            );
        });
    }

    #[test]
    fn mpsc_recv_option_multiple_then_close() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(4);
            mpsc_send(&tx, 1).await.expect("send 1");
            mpsc_send(&tx, 2).await.expect("send 2");
            drop(tx);
            assert_eq!(mpsc_recv_option(&mut rx).await, Some(1));
            assert_eq!(mpsc_recv_option(&mut rx).await, Some(2));
            assert_eq!(mpsc_recv_option(&mut rx).await, None);
        });
    }

    // ========================================================================
    // NEW TESTS: Watch channel edge cases
    // ========================================================================

    #[test]
    fn watch_multiple_sends_receiver_sees_latest() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = watch::channel(0);
            tx.send(1).expect("send 1");
            tx.send(2).expect("send 2");
            tx.send(3).expect("send 3");
            // Watch channels only retain the latest value
            assert_eq!(*rx.borrow(), 3);
        });
    }

    #[test]
    fn watch_send_after_drop_receiver_does_not_panic() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = watch::channel(0);
            drop(rx);
            // With no receivers, send may succeed (asupersync) or fail (tokio).
            // The important invariant is that it does not panic.
            let _result = tx.send(42);
        });
    }

    #[test]
    fn watch_initial_value_string() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (_, rx) = watch::channel(String::from("init"));
            assert_eq!(&*rx.borrow(), "init");
        });
    }

    #[test]
    fn watch_borrow_returns_ref_to_current_value() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = watch::channel(vec![1, 2, 3]);
            assert_eq!(*rx.borrow(), vec![1, 2, 3]);
            tx.send(vec![4, 5]).expect("send");
            assert_eq!(*rx.borrow(), vec![4, 5]);
        });
    }

    #[test]
    fn watch_multiple_receivers_see_same_value() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx1) = watch::channel(0);
            let rx2 = rx1.clone();
            tx.send(42).expect("send");
            assert_eq!(*rx1.borrow(), 42);
            assert_eq!(*rx2.borrow(), 42);
        });
    }

    // ========================================================================
    // NEW TESTS: Broadcast channel edge cases
    // ========================================================================

    #[test]
    fn broadcast_multiple_messages_fifo() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = broadcast::channel(16);
            broadcast_send(&tx, 1).expect("send 1");
            broadcast_send(&tx, 2).expect("send 2");
            broadcast_send(&tx, 3).expect("send 3");
            assert_eq!(broadcast_recv(&mut rx).await.expect("recv 1"), 1);
            assert_eq!(broadcast_recv(&mut rx).await.expect("recv 2"), 2);
            assert_eq!(broadcast_recv(&mut rx).await.expect("recv 3"), 3);
        });
    }

    #[test]
    fn broadcast_receiver_lagged_returns_error() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            // Create a tiny capacity channel
            let (tx, mut rx) = broadcast::channel(2);
            // Send more messages than the channel can hold
            broadcast_send(&tx, 1).expect("send 1");
            broadcast_send(&tx, 2).expect("send 2");
            broadcast_send(&tx, 3).expect("send 3");
            // First recv should return Lagged error
            let result = broadcast_recv(&mut rx).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn broadcast_send_with_no_receivers_returns_error() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = broadcast::channel::<i32>(16);
            drop(rx);
            // send should return error when there are no receivers
            let result = broadcast_send(&tx, 42);
            assert!(result.is_err());
        });
    }

    #[test]
    fn broadcast_subscribe_after_send_misses_prior_messages() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, _rx) = broadcast::channel(16);
            broadcast_send(&tx, 1).expect("send");
            let mut rx2 = tx.subscribe();
            broadcast_send(&tx, 2).expect("send 2");
            // rx2 subscribed after message 1, should only see message 2
            let val = broadcast_recv(&mut rx2).await.expect("recv");
            assert_eq!(val, 2);
        });
    }

    #[test]
    fn broadcast_try_recv_empty_channel() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (_tx, mut rx) = broadcast::channel::<i32>(16);
            let result = broadcast_try_recv(&mut rx);
            assert!(result.is_err());
            match result {
                Err(BroadcastTryRecvError::Empty) => {} // expected
                other => panic!("expected Empty, got {:?}", other),
            }
        });
    }

    #[test]
    fn broadcast_receiver_count_tracks_subscribers() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx1) = broadcast::channel::<i32>(16);
            assert_eq!(broadcast_receiver_count(&tx), 1);

            let rx2 = tx.subscribe();
            assert_eq!(broadcast_receiver_count(&tx), 2);

            drop(rx2);
            assert_eq!(broadcast_receiver_count(&tx), 1);

            drop(rx1);
            assert_eq!(broadcast_receiver_count(&tx), 0);
        });
    }

    // ========================================================================
    // NEW TESTS: Timeout edge cases
    // ========================================================================

    /// `Duration::ZERO` is not a non-blocking poll.
    ///
    /// The deadline is the instant the timeout future is *constructed*, so by
    /// the time it is first polled the ambient clock has normally passed it and
    /// the timeout reports `Elapsed` without ever polling the inner future. It
    /// only prefers ready inner work at exactly `now == deadline`.
    ///
    /// This test used to be `timeout_zero_duration_with_immediate_future_succeeds`
    /// and asserted the opposite. That assertion was a coin flip on whether the
    /// clock ticked between construction and first poll — it held on an idle
    /// machine and failed as soon as the suite ran to completion under load
    /// (ft-nam3s). So the load-independent half of the contract is what gets
    /// pinned here: a zero-duration timeout elapses rather than hanging.
    ///
    /// The rest of the repo already treats `Duration::ZERO` as invalid input —
    /// the search_bridge validation gate rejects such timeouts outright
    /// (br-ft-qfklb), and the snapshot scheduler shipped exactly this footgun as
    /// a shutdown poll and consequently never observed shutdown (ft-83kc7). Do
    /// not "fix" this back into a success assertion; use a real poll instead.
    #[test]
    fn timeout_zero_duration_elapses_rather_than_polling_inner_future() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            // `pending` can never complete, so `Ok` is unreachable by
            // construction and the outcome cannot depend on machine load.
            let result = timeout(Duration::ZERO, std::future::pending::<u32>()).await;
            assert!(
                result.is_err(),
                "a zero-duration timeout must elapse, never block"
            );
        });
    }

    #[test]
    fn timeout_returns_complex_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let result = timeout(Duration::from_secs(1), async { vec![1, 2, 3] }).await;
            assert_eq!(result.unwrap(), vec![1, 2, 3]);
        });
    }

    #[test]
    fn timeout_returns_result_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let result = timeout(Duration::from_secs(1), async { Ok::<_, String>(42) }).await;
            let inner = result.expect("should not timeout");
            assert_eq!(inner.unwrap(), 42);
        });
    }

    #[test]
    fn timeout_preserves_string_value() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let result = timeout(Duration::from_secs(1), async { String::from("survived") }).await;
            assert_eq!(result.unwrap(), "survived");
        });
    }

    // ========================================================================
    // NEW TESTS: Sleep edge cases
    // ========================================================================

    #[test]
    fn sleep_very_short_duration() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let start = std::time::Instant::now();
            sleep(Duration::from_nanos(1)).await;
            // Should complete quickly (nanos might round up to ~1ms)
            assert!(start.elapsed() < Duration::from_millis(500));
        });
    }

    #[test]
    fn sleep_one_millisecond() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let start = std::time::Instant::now();
            sleep(Duration::from_millis(1)).await;
            // Should complete in reasonable time
            assert!(start.elapsed() < Duration::from_millis(500));
        });
    }

    // ========================================================================
    // NEW TESTS: CompatRuntime with spawn_detached edge cases
    // ========================================================================

    #[test]
    fn spawn_detached_multiple_tasks() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        // Spawning multiple detached tasks should not panic
        for _ in 0..10 {
            rt.spawn_detached(async {});
        }
    }

    #[test]
    fn block_on_with_tokio_sync_inside() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = watch::channel(0);
            tx.send(42).expect("send");
            *rx.borrow()
        });
    }

    #[test]
    fn block_on_with_mutex_inside() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let m = Mutex::new(99);
            let guard = m.lock().await;
            *guard
        });
    }

    #[test]
    fn block_on_with_rwlock_inside() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let rw = RwLock::new(77);
            let guard = rw.read().await;
            *guard
        });
    }

    #[test]
    fn block_on_with_mpsc_inside() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(1);
            mpsc_send(&tx, 123).await.expect("send");
            mpsc_recv_option(&mut rx).await
        });
    }

    // ========================================================================
    // NEW TESTS: Type assertions and trait bounds
    // ========================================================================

    #[test]
    fn runtime_builder_build_error_type_is_string() {
        // The build() method returns Result<Runtime, String>
        let result = RuntimeBuilder::current_thread().build();
        let _rt: Runtime = result.expect("build should succeed");
    }

    #[test]
    fn semaphore_is_send_sync() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            // Verify Semaphore can be shared across tasks
            let sem = std::sync::Arc::new(Semaphore::new(1));
            let sem2 = sem.clone();
            let handle = task::spawn(async move {
                let _p = sem2.acquire().await.expect("acquire in spawned task");
            });
            handle.await.expect("spawned task should complete");
        });
    }

    #[test]
    fn mutex_is_send_sync() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            // Verify Mutex can be shared across tasks
            let m = std::sync::Arc::new(Mutex::new(0));
            let m2 = m.clone();
            let handle = task::spawn(async move {
                let mut guard = m2.lock().await;
                *guard = 42;
            });
            handle.await.expect("spawned task should complete");
            let guard = m.lock().await;
            assert_eq!(*guard, 42);
        });
    }

    #[test]
    fn rwlock_is_send_sync() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            // Verify RwLock can be shared across tasks
            let rw = std::sync::Arc::new(RwLock::new(0));
            let rw2 = rw.clone();
            let handle = task::spawn(async move {
                let mut guard = rw2.write().await;
                *guard = 99;
            });
            handle.await.expect("spawned task should complete");
            let guard = rw.read().await;
            assert_eq!(*guard, 99);
        });
    }

    // ========================================================================
    // Property-based tests
    // ========================================================================

    proptest! {
        #[test]
        fn proptest_mpsc_preserves_fifo(values in proptest::collection::vec(any::<i16>(), 0..64)) {
            let expected = values.clone();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let received = rt.block_on(async move {
                let (tx, mut rx) = mpsc::channel(expected.len().max(1));
                for value in &expected {
                    mpsc_send(&tx, *value).await.expect("send should succeed");
                }
                drop(tx);

                let mut out = Vec::with_capacity(expected.len());
                while let Some(value) = mpsc_recv_option(&mut rx).await {
                    out.push(value);
                }
                out
            });

            prop_assert_eq!(received, values);
        }

        #[test]
        fn proptest_watch_receiver_sees_latest(values in proptest::collection::vec(any::<u32>(), 1..64)) {
            let expected_latest = *values.last().expect("non-empty");
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed_latest = rt.block_on(async move {
                let (tx, rx) = watch::channel(values[0]);
                for value in values.iter().skip(1) {
                    tx.send(*value).expect("watch send should succeed");
                }
                *rx.borrow()
            });

            prop_assert_eq!(observed_latest, expected_latest);
        }

        #[test]
        fn proptest_semaphore_permit_accounting(
            permits in 1usize..16,
            acquire_count in 0usize..16,
        ) {
            prop_assume!(acquire_count <= permits);

            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let (during, after) = rt.block_on(async move {
                let sem = Semaphore::new(permits);
                let mut held = Vec::with_capacity(acquire_count);
                for _ in 0..acquire_count {
                    held.push(sem.acquire().await.expect("acquire should succeed"));
                }

                let during = sem.available_permits();
                drop(held);
                let after = sem.available_permits();
                (during, after)
            });

            prop_assert_eq!(during, permits - acquire_count);
            prop_assert_eq!(after, permits);
        }

        #[test]
        fn proptest_mutex_preserves_write_sequence(values in proptest::collection::vec(any::<i32>(), 0..128)) {
            let expected = values.clone();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed = rt.block_on(async move {
                let mutex = Mutex::new(Vec::<i32>::new());
                for value in &expected {
                    let mut guard = mutex.lock().await;
                    guard.push(*value);
                }
                let guard = mutex.lock().await;
                guard.clone()
            });

            prop_assert_eq!(observed, values);
        }

        #[test]
        fn proptest_rwlock_accumulates_deltas(
            initial in any::<i64>(),
            deltas in proptest::collection::vec(-1000i64..1000i64, 0..64),
        ) {
            let expected = initial + deltas.iter().copied().sum::<i64>();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed = rt.block_on(async move {
                let lock = RwLock::new(initial);
                for delta in &deltas {
                    let mut guard = lock.write().await;
                    *guard += *delta;
                }
                let guard = lock.read().await;
                *guard
            });

            prop_assert_eq!(observed, expected);
        }

        #[test]
        fn proptest_timeout_ready_future_returns_value(value in any::<i64>()) {
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed = rt.block_on(async move {
                timeout(Duration::from_millis(1), async move { value })
                    .await
                    .expect("ready future should not timeout")
            });

            prop_assert_eq!(observed, value);
        }

        #[test]
        fn proptest_spawn_blocking_returns_computed_result(values in proptest::collection::vec(any::<i32>(), 0..64)) {
            let expected: i64 = values.iter().map(|v| i64::from(*v)).sum();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed = rt.block_on(async move {
                spawn_blocking(move || values.iter().map(|v| i64::from(*v)).sum::<i64>())
                    .await
                    .expect("spawn_blocking should succeed")
            });

            prop_assert_eq!(observed, expected);
        }
    }

    // =========================================================================
    // Batch: DarkBadger wa-1u90p.7.1 — trait impls and edge cases
    // =========================================================================

    // -- TryAcquireError --

    #[test]
    fn try_acquire_error_debug_no_permits() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(0);
            let err = sem.try_acquire().unwrap_err();
            let dbg = format!("{:?}", err);
            assert!(!dbg.is_empty());
        });
    }

    #[test]
    fn try_acquire_error_debug_closed() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(5);
            sem.close();
            let err = sem.try_acquire().unwrap_err();
            let dbg = format!("{:?}", err);
            assert!(!dbg.is_empty());
        });
    }

    #[test]
    fn try_acquire_error_display_no_permits() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(0);
            let err = sem.try_acquire().unwrap_err();
            let display = format!("{}", err);
            assert!(!display.is_empty());
        });
    }

    #[test]
    fn try_acquire_error_display_closed() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(5);
            sem.close();
            let err = sem.try_acquire().unwrap_err();
            let display = format!("{}", err);
            assert!(!display.is_empty());
        });
    }

    #[test]
    fn try_acquire_error_is_std_error() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(0);
            let err = sem.try_acquire().unwrap_err();
            // Verify it implements std::error::Error
            let _: &dyn std::error::Error = &err;
        });
    }

    // -- AcquireError --

    #[test]
    fn acquire_error_debug() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(1);
            sem.close();
            let err = sem.acquire().await.unwrap_err();
            let dbg = format!("{:?}", err);
            assert!(!dbg.is_empty());
        });
    }

    #[test]
    fn acquire_error_display() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(1);
            sem.close();
            let err = sem.acquire().await.unwrap_err();
            let display = format!("{}", err);
            assert!(!display.is_empty());
        });
    }

    #[test]
    fn acquire_error_is_std_error() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(1);
            sem.close();
            let err = sem.acquire().await.unwrap_err();
            let _: &dyn std::error::Error = &err;
        });
    }

    // -- MutexGuard DerefMut edge cases --

    #[test]
    fn mutex_guard_deref_mut_vec_indexing() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let m = Mutex::new(vec![1, 2, 3]);
            {
                let mut guard = m.lock().await;
                guard[0] = 99;
                guard[2] = 77;
            }
            let guard = m.lock().await;
            assert_eq!(*guard, vec![99, 2, 77]);
        });
    }

    // -- RwLockWriteGuard DerefMut edge cases --

    #[test]
    fn rwlock_write_guard_deref_mut_vec_indexing() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let rw = RwLock::new(vec![10, 20, 30]);
            {
                let mut guard = rw.write().await;
                guard[1] = 99;
            }
            let guard = rw.read().await;
            assert_eq!(*guard, vec![10, 99, 30]);
        });
    }

    // -- spawn_blocking --

    #[test]
    fn spawn_blocking_basic_computation() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let result = spawn_blocking(|| 2 + 2).await;
            assert_eq!(result.unwrap(), 4);
        });
    }

    #[test]
    fn spawn_blocking_string_computation() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let result = spawn_blocking(|| {
                let mut s = String::new();
                for i in 0..5 {
                    s.push_str(&i.to_string());
                }
                s
            })
            .await;
            assert_eq!(result.unwrap(), "01234");
        });
    }

    #[test]
    fn spawn_blocking_heavy_computation() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let result = spawn_blocking(|| {
                let mut sum: u64 = 0;
                for i in 0..1000 {
                    sum += i;
                }
                sum
            })
            .await;
            assert_eq!(result.unwrap(), 499_500);
        });
    }

    /// br-ft-6qoxd: pre-cancel branch — cx already cancelled before
    /// `spawn_blocking_with_cx` is awaited. The helper must short-circuit
    /// without spawning the blocking work.
    #[test]
    fn spawn_blocking_with_cx_pre_cancel_short_circuits() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("br-ft-6qoxd pre-cancel"),
            );

            let work_ran = std::sync::Arc::new(AtomicBool::new(false));
            let work_ran_clone = std::sync::Arc::clone(&work_ran);

            let result: Result<u64, SpawnBlockingWithCxError> =
                spawn_blocking_with_cx(&cx, move || {
                    work_ran_clone.store(true, Ordering::SeqCst);
                    42
                })
                .await;

            assert!(
                matches!(
                    &result,
                    Err(SpawnBlockingWithCxError::CancelledBeforeSpawn {
                        kind: Some(crate::outcome::CancelKind::User)
                    })
                ),
                "pre-cancel must return the exact typed phase and kind; got: {result:?}"
            );
            assert!(
                !work_ran.load(Ordering::SeqCst),
                "blocking closure must not have been scheduled when pre-cancelled"
            );
        });
    }

    #[test]
    fn spawn_blocking_with_cx_uses_exact_explicit_identity_caps_and_cancellation() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();

        for (explicit_mask, expected_caps) in crate::cx::capability_mask_test_cases() {
            let explicit = {
                let _base_guard = crate::cx::Cx::set_current(Some(crate::cx::for_request()));
                let _restriction = crate::cx::Cx::push_restriction(explicit_mask);
                crate::cx::Cx::current().expect("restricted explicit blocking cx")
            };
            let ambient_mask = if expected_caps == [false; 5] {
                asupersync::cx::CapMask::all()
            } else {
                asupersync::cx::CapMask::none()
            };
            let ambient = {
                let _base_guard = crate::cx::Cx::set_current(Some(crate::cx::for_request()));
                let _restriction = crate::cx::Cx::push_restriction(ambient_mask);
                crate::cx::Cx::current().expect("restricted mismatched ambient cx")
            };
            ambient.cancel_with(
                crate::outcome::CancelKind::ParentCancelled,
                Some("SECRET ambient blocking context must not leak"),
            );

            let expected_identity = (explicit.region_id(), explicit.task_id());
            let ambient_identity = (ambient.region_id(), ambient.task_id());
            assert_ne!(
                expected_identity, ambient_identity,
                "identity test requires structurally distinct contexts"
            );

            let observed = rt.block_on(async move {
                task::spawn_with_cx(&ambient, move |_ambient_cx| async move {
                    let executor_thread = std::thread::current().id();
                    spawn_blocking_with_cx(&explicit, move || {
                        let active = crate::cx::Cx::current()
                            .expect("explicit cx installed inside blocking closure");
                        (
                            (active.region_id(), active.task_id()),
                            crate::cx::effective_capability_bits(&active),
                            active.checkpoint().is_err(),
                            std::thread::current().id(),
                            executor_thread,
                        )
                    })
                    .await
                })
                .await
                .expect("mismatched ambient task must settle")
            });
            let (identity, caps, cancelled, blocking_thread, executor_thread) =
                observed.expect("live explicit blocking cx must complete");
            assert_eq!(identity, expected_identity);
            assert_eq!(caps, expected_caps);
            assert!(
                !cancelled,
                "cancelled ambient cx must not replace the live explicit cx"
            );
            assert_ne!(
                blocking_thread, executor_thread,
                "driverless explicit/ambient contexts must not force blocking work inline"
            );
        }
    }

    #[test]
    fn spawn_blocking_with_cx_shares_midflight_cancel_state_with_closure() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cx = crate::cx::for_request();
            let expected_identity = (cx.region_id(), cx.task_id());
            let cx_for_helper = cx.clone();
            let (started_tx, started_rx) = oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (completed_tx, completed_rx) = oneshot::channel();

            let helper = task::spawn(async move {
                spawn_blocking_with_cx(&cx_for_helper, move || {
                    let active = crate::cx::Cx::current()
                        .expect("explicit cx installed before blocking wait");
                    let _ = started_tx.send((
                        (active.region_id(), active.task_id()),
                        crate::cx::effective_capability_bits(&active),
                        active.checkpoint().is_err(),
                    ));
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("midflight-cancel release signal");
                    let active_after = crate::cx::Cx::current()
                        .expect("explicit cx remains installed after blocking wait");
                    let delivery_cx = crate::cx::for_request();
                    let _ = completed_tx.send_with_cx(
                        &delivery_cx,
                        (
                            (active_after.region_id(), active_after.task_id()),
                            active_after.checkpoint().is_err(),
                        ),
                    );
                })
                .await
            });

            let before = timeout(Duration::from_secs(5), oneshot_recv(started_rx))
                .await
                .expect("blocking closure did not publish its start")
                .expect("blocking closure dropped its start signal");
            assert_eq!(before.0, expected_identity);
            assert_eq!(before.1, [true; 5]);
            assert!(!before.2);

            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("SECRET explicit midflight cancellation"),
            );
            release_tx
                .send(())
                .expect("blocking closure must retain its release receiver");

            let after = timeout(Duration::from_secs(5), oneshot_recv(completed_rx))
                .await
                .expect("blocking closure did not publish cancellation observation")
                .expect("blocking closure dropped its completion signal");
            assert_eq!(after.0, expected_identity);
            assert!(after.1, "blocking closure must share explicit Cx cancel state");

            let helper_result = helper.await.expect("blocking helper task must settle");
            assert!(
                matches!(
                    &helper_result,
                    Ok(())
                        | Err(SpawnBlockingWithCxError::CancelledMidFlight {
                            kind: Some(crate::outcome::CancelKind::User)
                        })
                ),
                "closure completion and cancellation watcher may race, but no other class is valid: {helper_result:?}"
            );
            if let Err(error) = &helper_result {
                assert!(!error.to_string().contains("SECRET"));
                assert!(!format!("{error:?}").contains("SECRET"));
            }
        });
    }

    /// ft-7p1bx: pins the blocking-pool regression directly. asupersync's
    /// `spawn_blocking` with an ambient `Cx` but NO blocking pool runs the
    /// closure INLINE on the executor thread — which froze the runtime for
    /// the duration of any blocking work (timers stalled, cancel watchers
    /// starved). The wrapper presets must configure a real pool so blocking
    /// work leaves the executor thread.
    #[test]
    fn spawn_blocking_runs_off_the_executor_thread() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let executor_thread = std::thread::current().id();
            let blocking_thread = spawn_blocking(std::thread::current)
                .await
                .expect("blocking closure should complete")
                .id();
            assert_ne!(
                executor_thread, blocking_thread,
                "spawn_blocking must run on the blocking pool, not inline on the \
                 executor thread (inline execution freezes timers and cancel watchers)"
            );
        });
    }

    /// br-ft-6qoxd: mid-flight cancel branch — cx cancels while the
    /// blocking work is still running. The helper must select-race the
    /// JoinHandle against the cx cancel watcher and resolve the await
    /// with a typed cancellation error. This test gates the closure until it is
    /// definitely running, so it continues on the blocking pool until natural
    /// return; queued work may instead be skipped on cancellation.
    ///
    /// Pattern mirrors `oneshot_recv_with_cx_mid_flight_cancel_via_select_race_pattern`
    /// at line 5710 and `distributed::race_with_cx_cancel` at tick 387.
    #[test]
    fn spawn_blocking_with_cx_mid_flight_cancel_via_select_race_pattern() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cx = crate::cx::Cx::for_testing();
            let cancel_trigger = cx.clone();
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (completed_tx, completed_rx) = oneshot::channel::<()>();

            let cancel_task = task::spawn(async move {
                await_test_signal(started_rx, "midflight-cancel closure start").await;
                cancel_trigger.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("br-ft-6qoxd mid-flight cancel"),
                );
            });

            let result = timeout(
                Duration::from_secs(2),
                spawn_blocking_with_cx(&cx, move || {
                    let _ = started_tx.send(());
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("midflight-cancel closure release signal");
                    let delivery_cx = crate::cx::for_request();
                    let _ = completed_tx.send_with_cx(&delivery_cx, ());
                    42
                }),
            )
            .await;
            release_tx
                .send(())
                .expect("midflight-cancel closure must retain its release receiver");
            await_test_signal(completed_rx, "midflight-cancel closure completion").await;
            cancel_task
                .await
                .expect("midflight cancellation trigger must settle");
            let result = result.expect("cancellation watcher did not settle within 2s");

            assert!(
                matches!(
                    &result,
                    Err(SpawnBlockingWithCxError::CancelledMidFlight {
                        kind: Some(crate::outcome::CancelKind::User)
                    })
                ),
                "mid-flight cancel must return the exact typed phase and kind; got: {result:?}"
            );
        });
    }

    #[test]
    fn spawn_blocking_with_cx_runtime_failure_is_not_cancellation() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cx = crate::cx::Cx::for_testing();
            let error = spawn_blocking_with_cx(&cx, || -> u64 {
                panic!("injected blocking closure panic")
            })
            .await
            .expect_err("blocking closure panic must become a typed runtime failure");

            assert!(matches!(error, SpawnBlockingWithCxError::RuntimeFailure));
            assert_eq!(error.to_string(), "blocking task runtime failure");
            assert!(
                !error
                    .to_string()
                    .contains("injected blocking closure panic")
            );
            assert!(!format!("{error:?}").contains("injected blocking closure panic"));
        });
    }

    #[test]
    fn spawn_blocking_with_cx_deadline_cancels_without_hot_looping() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let probe = crate::cx::Cx::for_testing();
            let budget = asupersync::types::Budget::new()
                .with_deadline(cx_timer_now(&probe) + Duration::from_millis(200));
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (completed_tx, completed_rx) = oneshot::channel::<()>();

            let started = std::time::Instant::now();
            let result = timeout(
                Duration::from_secs(2),
                spawn_blocking_with_cx(&cx, move || {
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("deadline closure release signal");
                    let delivery_cx = crate::cx::for_request();
                    let _ = completed_tx.send_with_cx(&delivery_cx, ());
                    42_u64
                }),
            )
            .await;
            release_tx
                .send(())
                .expect("deadline closure must retain its release receiver");
            await_test_signal(completed_rx, "deadline closure completion").await;
            let elapsed = started.elapsed();
            let result = result.expect("deadline watcher did not settle within 2s");

            assert!(
                matches!(
                    &result,
                    Err(SpawnBlockingWithCxError::CancelledMidFlight {
                        kind: Some(crate::outcome::CancelKind::Deadline)
                    })
                ),
                "finite Cx deadline must surface as typed mid-flight cancellation; got: {result:?}"
            );
            assert!(
                elapsed < Duration::from_secs(1),
                "deadline watcher must return before the blocking closure completes; took {elapsed:?}"
            );
        });
    }

    // -- task::spawn --

    #[test]
    fn task_spawn_returns_value() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let handle = task::spawn(async { 42 });
            let result = handle.await.expect("task should complete");
            assert_eq!(result, 42);
        });
    }

    #[test]
    fn task_spawn_string_value() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let handle = task::spawn(async { String::from("from task") });
            let result = handle.await.expect("task should complete");
            assert_eq!(result, "from task");
        });
    }

    #[test]
    fn task_spawn_with_cx_receives_explicit_context() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let cx = crate::cx::for_testing();
        let expected_region = cx.region_id();
        let expected_task = cx.task_id();
        rt.block_on(async move {
            let handle = task::spawn_with_cx(&cx, move |child_cx| async move {
                let active_before = crate::cx::Cx::current().expect("installed child cx");
                assert_eq!(active_before.region_id(), expected_region);
                assert_eq!(active_before.task_id(), expected_task);
                assert_eq!(child_cx.region_id(), expected_region);
                assert_eq!(child_cx.task_id(), expected_task);

                // Force a second poll and prove the scoped installation is
                // repeated rather than being a first-poll accident.
                task::yield_now().await;
                let active_after = crate::cx::Cx::current().expect("reinstalled child cx");
                assert_eq!(active_after.region_id(), expected_region);
                assert_eq!(active_after.task_id(), expected_task);
                active_after.checkpoint().expect("active cx checkpoint");
                child_cx.checkpoint().expect("child cx checkpoint");
                crate::runtime_async::current_runtime_handle().is_some()
            });
            let has_handle = handle.await.expect("task should complete");
            assert!(
                has_handle,
                "runtime handle should be available in spawned task"
            );
        });
    }

    #[test]
    fn joinset_spawn_with_cx_receives_explicit_context() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let cx = crate::cx::for_testing();
        let expected_region = cx.region_id();
        let expected_task = cx.task_id();
        rt.block_on(async move {
            let mut set = task::JoinSet::new();
            set.spawn_with_cx(&cx, move |child_cx| async move {
                let active = crate::cx::Cx::current().expect("installed child cx");
                assert_eq!(active.region_id(), expected_region);
                assert_eq!(active.task_id(), expected_task);
                assert_eq!(child_cx.region_id(), expected_region);
                assert_eq!(child_cx.task_id(), expected_task);
                active.checkpoint().expect("active cx checkpoint");
                child_cx.checkpoint().expect("child cx checkpoint");
                crate::runtime_async::current_runtime_handle().is_some()
            });

            let has_handle = set
                .join_next_with_cx(&cx)
                .await
                .expect("task result")
                .expect("join result");
            assert!(
                has_handle,
                "runtime handle should be available in spawned task"
            );
        });
    }

    #[test]
    fn task_spawn_with_cx_ambient_context_inherits_cancellation() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let cx = crate::cx::for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::ParentCancelled,
            Some("explicit spawn parent cancelled"),
        );

        rt.block_on(async move {
            let handle = task::spawn_with_cx(&cx, |_child_cx| async move {
                crate::cx::Cx::current()
                    .expect("installed child cx")
                    .checkpoint()
                    .is_err()
            });
            assert!(
                handle.await.expect("task should complete"),
                "ambient adapters must observe explicit parent cancellation"
            );
        });
    }

    #[test]
    fn task_spawn_with_cx_preserves_all_effective_capability_bits_across_yield() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        for (mask, expected) in crate::cx::capability_mask_test_cases() {
            let restricted = {
                let _base_guard = crate::cx::Cx::set_current(Some(crate::cx::for_testing()));
                let _restriction = crate::cx::Cx::push_restriction(mask);
                crate::cx::Cx::current().expect("restricted explicit cx")
            };
            assert_eq!(crate::cx::effective_capability_bits(&restricted), expected);

            rt.block_on(async move {
                let handle = task::spawn_with_cx(&restricted, move |child_cx| async move {
                    assert_eq!(crate::cx::effective_capability_bits(&child_cx), expected);
                    let before = crate::cx::effective_capability_bits(
                        &crate::cx::Cx::current().expect("installed restricted cx"),
                    );
                    task::yield_now().await;
                    let after = crate::cx::effective_capability_bits(
                        &crate::cx::Cx::current().expect("reinstalled restricted cx"),
                    );
                    (before, after)
                });
                assert_eq!(
                    handle.await.expect("task should complete"),
                    (expected, expected)
                );
            });
        }
    }

    #[test]
    fn task_try_spawn_with_cx_preserves_exact_identity_caps_and_cancellation() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();

        for (mask, expected) in crate::cx::capability_mask_test_cases() {
            let explicit = {
                let _base_guard = crate::cx::Cx::set_current(Some(crate::cx::for_request()));
                let _restriction = crate::cx::Cx::push_restriction(mask);
                crate::cx::Cx::current().expect("restricted explicit cx")
            };
            let expected_region = explicit.region_id();
            let expected_task = explicit.task_id();
            explicit.cancel_with(
                crate::outcome::CancelKind::ParentCancelled,
                Some("SECRET exact try-spawn cancellation"),
            );

            rt.block_on(async move {
                let handle = task::try_spawn_with_cx(&explicit, move |child_cx| async move {
                    let before = crate::cx::Cx::current().expect("installed try-spawn cx");
                    assert_eq!(before.region_id(), expected_region);
                    assert_eq!(before.task_id(), expected_task);
                    assert_eq!(child_cx.region_id(), expected_region);
                    assert_eq!(child_cx.task_id(), expected_task);
                    assert_eq!(crate::cx::effective_capability_bits(&before), expected);
                    assert_eq!(crate::cx::effective_capability_bits(&child_cx), expected);
                    assert!(before.checkpoint().is_err());

                    task::yield_now().await;
                    let after = crate::cx::Cx::current().expect("reinstalled try-spawn cx");
                    (
                        after.region_id(),
                        after.task_id(),
                        crate::cx::effective_capability_bits(&after),
                        after.checkpoint().is_err(),
                    )
                })
                .expect("installed runtime must admit try-spawn child");

                let observed = handle.await.expect("try-spawn child must settle");
                assert_eq!(observed.0, expected_region);
                assert_eq!(observed.1, expected_task);
                assert_eq!(observed.2, expected);
                assert!(observed.3);
            });
        }
    }

    #[test]
    fn task_try_spawn_with_cx_returns_typed_error_without_installed_handle() {
        let previous = current_runtime_handle();
        clear_runtime_handle();
        let cx = crate::cx::for_request();
        let result = task::try_spawn_with_cx(&cx, |_child_cx| async {});
        let unavailable = matches!(result, Err(task::SpawnError::RuntimeUnavailable));
        if let Some(previous) = previous {
            install_runtime_handle(previous);
        }
        assert!(
            unavailable,
            "missing installed handle must be a typed admission error, not a panic"
        );
    }

    #[test]
    fn nested_plain_and_join_set_spawn_never_regain_denied_capability_bits() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();

        for (mask, expected) in crate::cx::capability_mask_test_cases() {
            let restricted = {
                let _base_guard = crate::cx::Cx::set_current(Some(crate::cx::for_testing()));
                let _restriction = crate::cx::Cx::push_restriction(mask);
                crate::cx::Cx::current().expect("restricted explicit cx")
            };

            rt.block_on(async move {
                let outer = task::spawn_with_cx(&restricted, move |_child_cx| async move {
                    let plain = task::spawn(async move {
                        let before = crate::cx::effective_capability_bits(
                            &crate::cx::Cx::current().expect("plain nested spawn cx"),
                        );
                        task::yield_now().await;
                        let after = crate::cx::effective_capability_bits(
                            &crate::cx::Cx::current().expect("plain nested spawn cx after yield"),
                        );
                        (before, after)
                    })
                    .await
                    .expect("plain nested spawn must settle");

                    let mut set = task::JoinSet::new();
                    set.spawn(async move {
                        let before = crate::cx::effective_capability_bits(
                            &crate::cx::Cx::current().expect("JoinSet nested spawn cx"),
                        );
                        task::yield_now().await;
                        let after = crate::cx::effective_capability_bits(
                            &crate::cx::Cx::current().expect("JoinSet nested spawn cx after yield"),
                        );
                        (before, after)
                    });
                    let join_set = set
                        .join_next()
                        .await
                        .expect("JoinSet has one child")
                        .expect("JoinSet child must settle");
                    (plain, join_set)
                });

                assert_eq!(
                    outer.await.expect("explicit parent must settle"),
                    ((expected, expected), (expected, expected))
                );
            });
        }
    }

    #[test]
    fn nested_plain_spawn_inherits_explicit_parent_cancellation_and_identity() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let parent = crate::cx::for_testing();
        let expected_region = parent.region_id();
        let expected_task = parent.task_id();
        parent.cancel_with(
            crate::outcome::CancelKind::ParentCancelled,
            Some("SECRET nested plain spawn cancellation"),
        );

        rt.block_on(async move {
            let outer = task::spawn_with_cx(&parent, move |_child_cx| async move {
                task::spawn(async move {
                    let before = crate::cx::Cx::current().expect("plain nested inherited cx");
                    let identity_before = (before.region_id(), before.task_id());
                    let cancelled_before = before.checkpoint().is_err();
                    task::yield_now().await;
                    let after = crate::cx::Cx::current().expect("plain nested cx after yield");
                    (
                        identity_before,
                        (after.region_id(), after.task_id()),
                        cancelled_before,
                        after.checkpoint().is_err(),
                    )
                })
                .await
                .expect("plain nested task must settle")
            });

            let observed = outer.await.expect("explicit parent task must settle");
            assert_eq!(observed.0, (expected_region, expected_task));
            assert_eq!(observed.1, (expected_region, expected_task));
            assert!(observed.2 && observed.3);
        });
    }

    #[test]
    fn spawn_blocking_inherits_each_ambient_capability_bit() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();

        for (mask, expected) in crate::cx::capability_mask_test_cases() {
            let restricted = {
                let _base_guard = crate::cx::Cx::set_current(Some(crate::cx::for_testing()));
                let _restriction = crate::cx::Cx::push_restriction(mask);
                crate::cx::Cx::current().expect("restricted explicit cx")
            };

            rt.block_on(async move {
                let outer = task::spawn_with_cx(&restricted, move |_child_cx| async move {
                    task::spawn_blocking(move || {
                        crate::cx::effective_capability_bits(
                            &crate::cx::Cx::current().expect("blocking closure inherited cx"),
                        )
                    })
                    .await
                    .expect("blocking wrapper must settle")
                });
                assert_eq!(outer.await.expect("explicit parent must settle"), expected);
            });
        }
    }

    #[test]
    fn spawn_detached_never_regains_denied_capability_bits() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();

        for (mask, expected) in crate::cx::capability_mask_test_cases() {
            let (tx, rx) = oneshot::channel();
            {
                let _base_guard = crate::cx::Cx::set_current(Some(crate::cx::for_testing()));
                let _restriction = crate::cx::Cx::push_restriction(mask);
                rt.spawn_detached(async move {
                    let before = crate::cx::effective_capability_bits(
                        &crate::cx::Cx::current().expect("detached task cx"),
                    );
                    task::yield_now().await;
                    let after = crate::cx::effective_capability_bits(
                        &crate::cx::Cx::current().expect("detached task cx after yield"),
                    );
                    let _ = tx.send((before, after));
                });
            }

            let observed = rt.block_on(async {
                oneshot_recv(rx)
                    .await
                    .expect("detached capability probe must publish its result")
            });
            assert_eq!(observed, (expected, expected));
        }
    }

    // -- Semaphore permit count verification --

    #[test]
    fn semaphore_multiple_try_acquire_exhaust_permits() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(3);
            let _p1 = sem.try_acquire().expect("1st acquire");
            let _p2 = sem.try_acquire().expect("2nd acquire");
            let _p3 = sem.try_acquire().expect("3rd acquire");
            assert_eq!(sem.available_permits(), 0);
            assert!(sem.try_acquire().is_err());
        });
    }

    // -- Channel edge cases --

    #[test]
    fn watch_channel_drop_sender_borrow_still_works() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = watch::channel(42);
            tx.send(100).expect("send");
            drop(tx);
            // After sender dropped, receiver should still see last value
            assert_eq!(*rx.borrow(), 100);
        });
    }

    #[test]
    fn broadcast_receiver_clone_both_receive() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx1) = broadcast::channel(16);
            let mut rx2 = tx.subscribe();
            broadcast_send(&tx, 7).expect("send");
            assert_eq!(broadcast_recv(&mut rx1).await.expect("r1"), 7);
            assert_eq!(broadcast_recv(&mut rx2).await.expect("r2"), 7);
        });
    }

    // ========================================================================
    // Notify tests
    // ========================================================================

    #[test]
    fn notify_one_wakes_waiter() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let n = notify::Notify::new();
            let n2 = std::sync::Arc::new(n);
            let n3 = n2.clone();

            let handle = task::spawn(async move {
                n3.notified().await;
                42
            });

            sleep(Duration::from_millis(5)).await;
            n2.notify_one();

            let result = handle.await.expect("task");
            assert_eq!(result, 42);
        });
    }

    #[test]
    fn notify_waiters_wakes_all() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let n = std::sync::Arc::new(notify::Notify::new());
            let n1 = n.clone();
            let n2 = n.clone();

            let h1 = task::spawn(async move {
                n1.notified().await;
                1
            });
            let h2 = task::spawn(async move {
                n2.notified().await;
                2
            });

            sleep(Duration::from_millis(5)).await;
            n.notify_waiters();

            let r1 = h1.await.expect("h1");
            let r2 = h2.await.expect("h2");
            assert_eq!(r1 + r2, 3);
        });
    }

    #[test]
    fn notify_before_notified_does_not_block() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let n = notify::Notify::new();
            n.notify_one();
            // Should complete immediately since notification is stored
            n.notified().await;
        });
    }

    #[test]
    fn notify_new_does_not_panic() {
        let _n = notify::Notify::new();
    }

    // ========================================================================
    // Oneshot channel tests
    // ========================================================================

    #[test]
    fn oneshot_send_recv() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel();
            oneshot_send(tx, 42).expect("send");
            let val = oneshot_recv(rx).await.expect("recv");
            assert_eq!(val, 42);
        });
    }

    #[test]
    fn oneshot_recv_after_drop_sender_returns_err() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<u32>();
            drop(tx);
            let result = oneshot_recv(rx).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn oneshot_send_after_drop_receiver_returns_err() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<u32>();
            drop(rx);
            let result = oneshot_send(tx, 42);
            assert!(result.is_err());
        });
    }

    #[test]
    fn oneshot_with_string_payload() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel();
            oneshot_send(tx, "hello".to_string()).expect("send");
            let val = oneshot_recv(rx).await.expect("recv");
            assert_eq!(val, "hello");
        });
    }

    #[test]
    fn oneshot_with_result_payload() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<Result<i32, String>>();
            oneshot_send(tx, Ok(99)).expect("send");
            let val = oneshot_recv(rx).await.expect("recv");
            assert_eq!(val.unwrap(), 99);
        });
    }

    #[test]
    fn oneshot_with_result_err_payload() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<Result<i32, String>>();
            oneshot_send(tx, Err("fail".to_string())).expect("send");
            let val = oneshot_recv(rx).await.expect("recv");
            assert_eq!(val.unwrap_err(), "fail");
        });
    }

    #[test]
    fn oneshot_with_vec_payload() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel();
            oneshot_send(tx, vec![1, 2, 3]).expect("send");
            let val = oneshot_recv(rx).await.expect("recv");
            assert_eq!(val, vec![1, 2, 3]);
        });
    }

    #[test]
    fn oneshot_with_option_payload() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<Option<u32>>();
            oneshot_send(tx, Some(7)).expect("send");
            assert_eq!(oneshot_recv(rx).await.expect("recv"), Some(7));

            let (tx2, rx2) = oneshot::channel::<Option<u32>>();
            oneshot_send(tx2, None).expect("send none");
            assert_eq!(oneshot_recv(rx2).await.expect("recv none"), None);
        });
    }

    #[test]
    fn oneshot_recv_error_is_recv_error() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<u32>();
            drop(tx);
            let err = oneshot_recv(rx).await.unwrap_err();
            assert!(!err.is_empty());
        });
    }

    #[test]
    fn oneshot_send_returns_error_on_closed_receiver() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<u32>();
            drop(rx);
            let result = oneshot_send(tx, 42);
            assert!(result.is_err());
        });
    }

    fn oneshot_waker_poison_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    fn oneshot_receiver_waker_poison_counter_is_observable_and_clean() {
        let _guard = oneshot_waker_poison_test_lock();
        oneshot::reset_receiver_waker_lock_poisoned_count_for_test();
        let (tx, rx) = oneshot::channel::<u32>();
        let mut receive = Box::pin(oneshot_recv(rx));
        let noop = futures::task::noop_waker();
        let mut caller_cx = std::task::Context::from_waker(&noop);
        assert!(matches!(
            receive.as_mut().poll(&mut caller_cx),
            std::task::Poll::Pending
        ));
        assert_eq!(tx.send(40), Ok(()));
        assert!(matches!(
            receive.as_mut().poll(&mut caller_cx),
            std::task::Poll::Ready(Ok(40))
        ));
        assert_eq!(
            oneshot::receiver_waker_lock_poisoned_count(),
            0,
            "clean oneshot forwarding must not report a poisoned slot"
        );
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn oneshot_send_contains_panicking_receiver_waker_and_remains_receivable() {
        let (tx, rx) = oneshot::channel::<u32>();
        let mut receive = Box::pin(oneshot_recv(rx));
        let (probe, panicking_waker) = probe_waker(true);
        let mut panicking_cx = std::task::Context::from_waker(&panicking_waker);
        assert!(matches!(
            receive.as_mut().poll(&mut panicking_cx),
            std::task::Poll::Pending
        ));

        let send = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tx.send(41)));
        assert!(matches!(send, Ok(Ok(()))));
        assert_eq!(probe.count(), 1);

        let noop = futures::task::noop_waker();
        let mut noop_cx = std::task::Context::from_waker(&noop);
        assert!(matches!(
            receive.as_mut().poll(&mut noop_cx),
            std::task::Poll::Ready(Ok(41))
        ));
    }

    #[test]
    fn oneshot_receiver_waker_replacement_wakes_only_latest_caller() {
        let (tx, rx) = oneshot::channel::<u32>();
        let mut receive = Box::pin(oneshot_recv(rx));
        let (probe_a, waker_a) = probe_waker(false);
        let (probe_b, waker_b) = probe_waker(false);
        let mut cx_a = std::task::Context::from_waker(&waker_a);
        let mut cx_b = std::task::Context::from_waker(&waker_b);

        assert!(matches!(
            receive.as_mut().poll(&mut cx_a),
            std::task::Poll::Pending
        ));
        assert!(matches!(
            receive.as_mut().poll(&mut cx_b),
            std::task::Poll::Pending
        ));
        assert_eq!(tx.send(42), Ok(()));
        assert_eq!(probe_a.count(), 0);
        assert_eq!(probe_b.count(), 1);

        let noop = futures::task::noop_waker();
        let mut noop_cx = std::task::Context::from_waker(&noop);
        assert!(matches!(
            receive.as_mut().poll(&mut noop_cx),
            std::task::Poll::Ready(Ok(42))
        ));
    }

    #[test]
    fn dropping_pending_oneshot_receive_never_wakes_stale_caller() {
        let (tx, rx) = oneshot::channel::<u32>();
        let mut receive = Box::pin(oneshot_recv(rx));
        let (probe, waker) = probe_waker(false);
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            receive.as_mut().poll(&mut cx),
            std::task::Poll::Pending
        ));

        drop(receive);
        assert_eq!(tx.send(43), Err(43));
        assert_eq!(probe.count(), 0);
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn oneshot_send_during_outer_unwind_cannot_double_panic() {
        struct SendOnDrop {
            sender: Option<oneshot::Sender<u32>>,
        }

        impl Drop for SendOnDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.sender.take() {
                    let _ = sender.send(44);
                }
            }
        }

        let (tx, rx) = oneshot::channel::<u32>();
        let mut receive = Box::pin(oneshot_recv(rx));
        let (probe, panicking_waker) = probe_waker(true);
        let mut panicking_cx = std::task::Context::from_waker(&panicking_waker);
        assert!(matches!(
            receive.as_mut().poll(&mut panicking_cx),
            std::task::Poll::Pending
        ));

        let outer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _send_on_drop = SendOnDrop { sender: Some(tx) };
            panic!("synthetic outer panic");
        }));
        assert!(
            outer.is_err(),
            "the original outer panic must remain visible"
        );
        assert_eq!(probe.count(), 1);

        let noop = futures::task::noop_waker();
        let mut noop_cx = std::task::Context::from_waker(&noop);
        assert!(matches!(
            receive.as_mut().poll(&mut noop_cx),
            std::task::Poll::Ready(Ok(44))
        ));
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn oneshot_receive_drop_contains_caller_waker_drop_during_outer_unwind() {
        let (tx, rx) = oneshot::channel::<u32>();
        let mut receive = Box::pin(oneshot_recv(rx));
        let (drop_count, drop_panicking) = drop_panicking_waker();
        {
            let mut caller_cx = std::task::Context::from_waker(&drop_panicking);
            assert!(matches!(
                receive.as_mut().poll(&mut caller_cx),
                std::task::Poll::Pending
            ));
        }
        drop(drop_panicking);

        let outer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _receive_dropped_during_unwind = receive;
            panic!("synthetic outer panic while dropping oneshot receive");
        }));
        assert!(
            outer.is_err(),
            "the original outer panic must remain visible"
        );
        assert_eq!(
            drop_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "caller waker drop panic must be quarantined exactly once"
        );
        assert_eq!(tx.send(45), Err(45));
    }

    // ========================================================================
    // Process module tests
    // ========================================================================

    #[test]
    fn process_command_echo() {
        let output = std::process::Command::new("echo")
            .arg("hello")
            .output()
            .expect("echo should succeed");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello"));
    }

    #[test]
    fn process_command_false_returns_non_zero() {
        let output = std::process::Command::new("false")
            .output()
            .expect("false should execute");
        assert!(!output.status.success());
    }

    #[test]
    fn process_command_with_env() {
        let output = std::process::Command::new("env")
            .env("TEST_RC_VAR", "42")
            .output()
            .expect("env should succeed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("TEST_RC_VAR=42"));
    }

    #[test]
    fn process_command_stdin_piped() {
        use std::process::Stdio;
        let child = std::process::Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        assert!(child.is_ok());
        // Clean up the spawned process
        if let Ok(mut c) = child {
            let _ = c.kill();
        }
    }

    #[test]
    fn process_command_nonexistent_binary() {
        let result = std::process::Command::new("nonexistent_binary_xyz_123").output();
        assert!(result.is_err());
    }

    #[test]
    fn process_command_args_multiple() {
        let output = std::process::Command::new("echo")
            .args(["a", "b", "c"])
            .output()
            .expect("echo should succeed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("a b c"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_signal_helper_uses_fixed_kill_path_and_rejects_injected_signal() {
        let kill_path = process::unix_kill_command();
        assert!(
            matches!(kill_path, "/bin/kill" | "/usr/bin/kill"),
            "kill helper must use a fixed absolute path, got {kill_path}"
        );
        assert!(kill_path.starts_with('/'));

        let err = process::send_unix_signal_to_pid(i64::from(std::process::id()), "TERM;sh")
            .expect_err("signal names must not be shell-like or option-like strings");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn unix_signal_helper_can_probe_current_process() {
        let status = process::send_unix_signal_to_pid(i64::from(std::process::id()), "0")
            .expect("signal 0 should probe the current process");
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn unix_signal_helper_can_probe_an_exact_process_group() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 10"]);
        process::configure_process_group(&mut command)
            .expect("test child must own a distinct process group");
        let mut child = command.spawn().expect("spawn process-group probe child");

        let probe = process::send_unix_signal_to_process_group(child.id(), "0");
        let _ = child.kill();
        let _ = child.wait();

        let status = probe.expect("signal 0 should probe the exact child process group");
        assert!(status.success());
    }

    #[cfg(all(feature = "asupersync-runtime", unix))]
    #[test]
    fn process_command_kill_on_drop_stops_timed_out_child() {
        let marker_dir = tempfile::tempdir().expect("tempdir");
        let marker_path = marker_dir.path().join("should_not_exist.txt");
        let script = "(sleep 1; echo done > \"$FT_RUNTIME_COMPAT_MARKER\") & wait";
        let rt = RuntimeBuilder::current_thread().build().unwrap();

        rt.block_on(async {
            let mut cmd = process::Command::new("sh");
            cmd.arg("-c");
            cmd.arg(script);
            cmd.env("FT_RUNTIME_COMPAT_MARKER", &marker_path);
            cmd.kill_on_drop(true);

            let result = timeout(Duration::from_millis(50), cmd.output()).await;
            assert!(result.is_err(), "command should time out");

            sleep(Duration::from_millis(1500)).await;
        });

        assert!(
            !marker_path.exists(),
            "timed-out child should have been terminated before writing output"
        );
    }

    /// ft-xbnl0.2.3 Cx-first: `Command::output_with_cx` must
    /// bridge caller-cx cancellation into the underlying
    /// process-polling worker within ~PROCESS_POLL_INTERVAL (10ms).
    /// This test spawns a `sleep 10`, cancels the cx after 100ms,
    /// and asserts the call returns with `ErrorKind::Interrupted`
    /// in well under the 10s the child would otherwise run.
    #[cfg(all(feature = "asupersync-runtime", unix))]
    #[test]
    fn process_command_output_with_cx_cancellation_surfaces_as_interrupted() {
        // Thread-isolated to prevent TLS interference from 25K+ parallel tests.
        run_async_test_isolated(|| async {
            let cx = crate::cx::for_testing();
            let cx_cancel_trigger = cx.clone();
            task::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                cx_cancel_trigger.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("process_command_output_with_cx test cancel"),
                );
            });

            let mut cmd = process::Command::new("sh");
            cmd.arg("-c");
            cmd.arg("sleep 10");
            cmd.kill_on_drop(true);

            let start = std::time::Instant::now();
            let result = cmd.output_with_cx(&cx).await;
            let elapsed = start.elapsed();

            let err = result.expect_err("cancelled cx should surface as IO error");
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::Interrupted,
                "cx-cancelled process output must surface as Interrupted: {err}"
            );
            assert!(
                elapsed < Duration::from_secs(5),
                "cancellation should surface promptly (got {elapsed:?}); the 10s sleep would dominate if cx was ignored"
            );
        });
    }

    #[cfg(all(feature = "asupersync-runtime", unix))]
    #[test]
    fn process_command_output_with_cx_timeout_settles_process_tree_before_return() {
        run_async_test_isolated(|| async {
            let artifact_dir = tempfile::tempdir().expect("timeout settlement tempdir");
            let pid_path = artifact_dir.path().join("leader.pid");
            let delayed_marker_path = artifact_dir.path().join("must-not-exist");
            let cx = crate::cx::for_testing();
            let mut command = process::Command::new("sh");
            command
                .arg("-c")
                .arg(
                    "printf '%s' \"$$\" > \"$FT_RUNTIME_COMPAT_PID\"; \
                     (sleep 2; printf leaked > \"$FT_RUNTIME_COMPAT_MARKER\") & wait",
                )
                .env("FT_RUNTIME_COMPAT_PID", &pid_path)
                .env("FT_RUNTIME_COMPAT_MARKER", &delayed_marker_path)
                .kill_on_drop(true);

            let started = std::time::Instant::now();
            let error = command
                .output_with_cx_timeout(&cx, Duration::from_millis(500))
                .await
                .expect_err("finite supervisor deadline must stop the process tree");
            let elapsed = started.elapsed();
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            assert!(
                CommandTimedOut::from_io_error(&error).is_some(),
                "deadline failure must retain its typed content-free receipt"
            );
            assert!(
                elapsed < Duration::from_secs(2),
                "settled timeout must not wait for the child's natural exit: {elapsed:?}"
            );

            let process_id: i64 = std::fs::read_to_string(&pid_path)
                .expect("child must publish its process id before the deadline")
                .parse()
                .expect("published process id must be numeric");
            let probe = process::send_unix_signal_to_pid(process_id, "0")
                .expect("process-existence probe must execute");
            assert!(
                !probe.success(),
                "timed-out process leader must already be reaped when the API returns"
            );

            sleep(Duration::from_millis(1_750)).await;
            assert!(
                !delayed_marker_path.exists(),
                "timed-out process descendants must not survive to perform delayed effects"
            );
        });
    }

    #[cfg(all(feature = "asupersync-runtime", unix))]
    #[test]
    fn process_command_output_with_cx_deadline_surfaces_as_interrupted() {
        run_async_test_isolated(|| async {
            let probe = crate::cx::Cx::for_testing();
            let budget = asupersync::types::Budget::new()
                .with_deadline(cx_timer_now(&probe) + Duration::from_millis(40));
            let cx = crate::cx::Cx::for_testing_with_budget(budget);

            let mut cmd = process::Command::new("sh");
            cmd.arg("-c");
            cmd.arg("sleep 2");
            cmd.kill_on_drop(true);

            let started = std::time::Instant::now();
            let result = cmd.output_with_cx(&cx).await;
            let elapsed = started.elapsed();

            let error = result.expect_err("Cx deadline must stop the child process");
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::Interrupted,
                "deadline-cancelled process output must surface as Interrupted: {error}"
            );
            assert!(
                elapsed < Duration::from_millis(1500),
                "deadline must stop the child before its 2 s sleep completes; took {elapsed:?}"
            );
        });
    }

    #[cfg(all(feature = "asupersync-runtime", unix))]
    #[test]
    fn process_command_output_matches_stdlib_stdin_null_behavior() {
        let script = "if [ /dev/null -ef /dev/fd/0 ]; then printf null; else printf inherited; fi";
        let expected = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("stdlib command should succeed");
        assert_eq!(String::from_utf8_lossy(&expected.stdout), "null");

        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let mut cmd = process::Command::new("sh");
            cmd.arg("-c");
            cmd.arg(script);

            let output = cmd
                .output()
                .await
                .expect("runtime_async command should succeed");
            assert_eq!(output.stdout, expected.stdout);
            assert_eq!(output.stderr, expected.stderr);
        });
    }

    // ========================================================================
    // IO module tests
    // ========================================================================

    #[test]
    fn io_async_write_ext_available() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            use io::AsyncWriteExt;
            let mut buf = Vec::new();
            buf.write_all(b"test").await.expect("write should succeed");
            assert_eq!(&buf, b"test");
        });
    }

    // ========================================================================
    // Net module tests
    // ========================================================================

    #[test]
    fn net_tcp_listener_bind() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let listener = net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind should succeed");
            let addr = listener.local_addr().expect("should have local addr");
            assert!(addr.port() > 0);
        });
    }

    #[test]
    fn net_tcp_stream_connect_to_listener() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let listener = net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("local addr");

            let stream = net::TcpStream::connect(addr).await.expect("connect");
            stream.shutdown(std::net::Shutdown::Both).expect("shutdown");
        });
    }

    #[test]
    fn net_tcp_roundtrip() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            use io::{AsyncReadExt, AsyncWriteExt};

            let listener = net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let server = task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut buf = [0u8; 4];
                stream.read_exact(&mut buf).await.expect("read");
                buf
            });

            let mut client = net::TcpStream::connect(addr).await.expect("connect");
            client.write_all(b"ping").await.expect("write");
            client
                .shutdown(std::net::Shutdown::Write)
                .expect("shutdown");

            let received = server.await.expect("server task");
            assert_eq!(&received, b"ping");
        });
    }

    // ========================================================================
    // RuntimeBuilder enable_all and thread_name tests
    // ========================================================================

    #[test]
    fn runtime_builder_enable_all_is_chainable() {
        let rt = RuntimeBuilder::multi_thread().enable_all().build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_thread_name_is_chainable() {
        let rt = RuntimeBuilder::multi_thread()
            .thread_name("test-worker")
            .build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_full_chain() {
        let rt = RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("full-chain-test")
            .build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_current_thread_with_enable_all_and_thread_name() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .thread_name("ct-test")
            .build();
        assert!(rt.is_ok());
    }

    // ========================================================================
    // task::spawn_blocking tests
    // ========================================================================

    #[test]
    fn task_spawn_blocking_returns_value() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let handle = task::spawn_blocking(|| 42);
            let result = handle.await.expect("join");
            assert_eq!(result, 42);
        });
    }

    #[test]
    fn task_spawn_blocking_runs_closure() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let handle = task::spawn_blocking(|| {
                let mut sum = 0;
                for i in 0..100 {
                    sum += i;
                }
                sum
            });
            assert_eq!(handle.await.expect("join"), 4950);
        });
    }

    #[test]
    fn task_spawn_blocking_preserves_exact_context_and_never_runs_inline() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();

        for (mask, expected) in crate::cx::capability_mask_test_cases() {
            let explicit = {
                let _base_guard = crate::cx::Cx::set_current(Some(crate::cx::for_request()));
                let _restriction = crate::cx::Cx::push_restriction(mask);
                crate::cx::Cx::current().expect("restricted blocking cx")
            };
            let expected_identity = (explicit.region_id(), explicit.task_id());

            let observed = rt.block_on(async move {
                task::spawn_with_cx(&explicit, move |_child_cx| async move {
                    let executor_thread = std::thread::current().id();
                    task::spawn_blocking(move || {
                        let active = crate::cx::Cx::current()
                            .expect("blocking closure must inherit exact ambient cx");
                        (
                            (active.region_id(), active.task_id()),
                            crate::cx::effective_capability_bits(&active),
                            std::thread::current().id(),
                            executor_thread,
                        )
                    })
                    .await
                    .expect("blocking child must settle")
                })
                .await
                .expect("explicit-cx parent must settle")
            });

            assert_eq!(observed.0, expected_identity);
            assert_eq!(observed.1, expected);
            assert_ne!(
                observed.2, observed.3,
                "driverless explicit Cx must not force task::spawn_blocking inline"
            );
        }
    }

    #[test]
    fn task_spawn_blocking_abort_stops_delivery_but_not_a_started_closure() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (completion_tx, completion_rx) = oneshot::channel::<()>();
            let finished_by_closure = std::sync::Arc::clone(&finished);

            let handle = task::spawn_blocking(move || {
                let _ = started_tx.send(());
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("blocking-closure release signal");
                finished_by_closure.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = completion_tx.send(());
                "done"
            });

            await_test_signal(started_rx, "blocking-closure start").await;

            handle.abort();
            let result = timeout(Duration::from_secs(5), handle).await;
            let inner = result.expect("handle.await did not resolve within 5s after abort");
            assert!(
                matches!(inner, Err(ref error) if error.kind() == task::JoinErrorKind::Aborted),
                "a started blocking closure held behind a gate cannot win the abort race"
            );
            assert!(
                !finished.load(std::sync::atomic::Ordering::SeqCst),
                "aborted wrapper acknowledgement must not falsely imply OS work stopped"
            );

            release_tx
                .send(())
                .expect("started blocking closure must retain its release receiver");
            await_test_signal(completion_rx, "blocking-closure completion").await;
            assert!(
                finished.load(std::sync::atomic::Ordering::SeqCst),
                "the already-started closure must remain alive and finish naturally"
            );
        });
    }

    #[test]
    fn task_abort_wakes_pending_waiter() {
        use futures::task::{ArcWake, waker_ref};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingWaker {
            wake_count: AtomicUsize,
        }

        impl ArcWake for CountingWaker {
            fn wake_by_ref(arc_self: &Arc<Self>) {
                arc_self.wake_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let handle = task::spawn(std::future::poll_fn(|_| std::task::Poll::<()>::Pending));
            let wake_counter = Arc::new(CountingWaker {
                wake_count: AtomicUsize::new(0),
            });
            let waker = waker_ref(&wake_counter);
            let mut cx = std::task::Context::from_waker(&waker);
            let mut pinned = std::pin::pin!(handle);

            assert!(matches!(
                pinned.as_mut().poll(&mut cx),
                std::task::Poll::Pending
            ));

            pinned.as_ref().get_ref().abort();

            assert!(
                wake_counter.wake_count.load(Ordering::SeqCst) >= 1,
                "abort() should wake the current waiter"
            );

            let result = pinned.as_mut().await;
            assert!(matches!(result, Err(ref err) if err.is_cancelled()));
        });
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn task_abort_contains_panicking_waker_and_preserves_cancellation() {
        use std::sync::Arc;
        use std::task::{Wake, Waker};

        struct PanickingWaker;

        impl Wake for PanickingWaker {
            fn wake(self: Arc<Self>) {
                panic!("executor waker panic must remain contained");
            }

            fn wake_by_ref(self: &Arc<Self>) {
                panic!("executor waker panic must remain contained");
            }
        }

        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let handle = task::spawn(std::future::poll_fn(|_| std::task::Poll::<()>::Pending));
            let waker = Waker::from(Arc::new(PanickingWaker));
            let mut cx = std::task::Context::from_waker(&waker);
            let mut pinned = std::pin::pin!(handle);

            assert!(matches!(
                pinned.as_mut().poll(&mut cx),
                std::task::Poll::Pending
            ));

            let abort_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pinned.as_ref().get_ref().abort();
            }));
            assert!(
                abort_result.is_ok(),
                "an executor waker panic must not escape task abort"
            );

            let result = pinned.as_mut().await;
            assert!(matches!(result, Err(ref error) if error.is_cancelled()));
        });
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn task_completion_contains_panicking_caller_waker_then_repolls_ready() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = futures::channel::oneshot::channel::<u32>();
            let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed_by_task = std::sync::Arc::clone(&completed);
            let (completed_tx, completed_rx) = oneshot::channel::<()>();
            let mut handle = Box::pin(task::spawn(async move {
                let value = rx.await.expect("completion gate sender");
                completed_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = completed_tx.send(());
                value
            }));
            let (probe, panicking_waker) = probe_waker(true);
            let mut panicking_cx = std::task::Context::from_waker(&panicking_waker);
            assert!(matches!(
                handle.as_mut().poll(&mut panicking_cx),
                std::task::Poll::Pending
            ));

            tx.send(51).expect("open completion gate");
            await_test_signal(completed_rx, "panicking-waker task completion").await;
            assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(probe.count(), 1);

            let noop = futures::task::noop_waker();
            let mut noop_cx = std::task::Context::from_waker(&noop);
            assert!(matches!(
                handle.as_mut().poll(&mut noop_cx),
                std::task::Poll::Ready(Ok(51))
            ));
        });
    }

    #[test]
    fn task_completion_waker_replacement_wakes_only_latest_caller() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = futures::channel::oneshot::channel::<u32>();
            let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed_by_task = std::sync::Arc::clone(&completed);
            let (completed_tx, completed_rx) = oneshot::channel::<()>();
            let mut handle = Box::pin(task::spawn(async move {
                let value = rx.await.expect("completion gate sender");
                completed_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = completed_tx.send(());
                value
            }));
            let (probe_a, waker_a) = probe_waker(false);
            let (probe_b, waker_b) = probe_waker(false);
            let mut cx_a = std::task::Context::from_waker(&waker_a);
            let mut cx_b = std::task::Context::from_waker(&waker_b);
            assert!(matches!(
                handle.as_mut().poll(&mut cx_a),
                std::task::Poll::Pending
            ));
            assert!(matches!(
                handle.as_mut().poll(&mut cx_b),
                std::task::Poll::Pending
            ));

            tx.send(52).expect("open completion gate");
            await_test_signal(completed_rx, "replacement-waker task completion").await;
            assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(probe_a.count(), 0);
            assert_eq!(probe_b.count(), 1);

            let noop = futures::task::noop_waker();
            let mut noop_cx = std::task::Context::from_waker(&noop);
            assert!(matches!(
                handle.as_mut().poll(&mut noop_cx),
                std::task::Poll::Ready(Ok(52))
            ));
        });
    }

    #[test]
    fn dropping_pending_task_handle_never_wakes_stale_caller_on_detached_completion() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = futures::channel::oneshot::channel::<u32>();
            let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed_by_task = std::sync::Arc::clone(&completed);
            let (completed_tx, completed_rx) = oneshot::channel::<()>();
            let mut handle = Box::pin(task::spawn(async move {
                let value = rx.await.expect("completion gate sender");
                completed_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = completed_tx.send(());
                value
            }));
            let (probe, waker) = probe_waker(false);
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(matches!(
                handle.as_mut().poll(&mut cx),
                std::task::Poll::Pending
            ));

            drop(handle);
            tx.send(53).expect("open completion gate");
            await_test_signal(completed_rx, "detached task completion").await;
            assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(probe.count(), 0);
        });
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn task_abort_drops_pending_future_before_join_acknowledgement() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = futures::channel::oneshot::channel::<u32>();
            let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed_by_task = std::sync::Arc::clone(&completed);
            let mut handle = Box::pin(task::spawn(async move {
                let value = rx.await.expect("completion gate sender");
                completed_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
                value
            }));
            let (probe, panicking_waker) = probe_waker(true);
            let mut panicking_cx = std::task::Context::from_waker(&panicking_waker);
            assert!(matches!(
                handle.as_mut().poll(&mut panicking_cx),
                std::task::Poll::Pending
            ));

            let abort = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle.as_ref().get_ref().abort();
            }));
            assert!(abort.is_ok(), "abort wake must remain contained");
            assert_eq!(probe.count(), 1);

            let result = handle.as_mut().await;
            assert!(matches!(result, Err(ref error) if error.is_cancelled()));
            assert!(
                tx.send(54).is_err(),
                "abort acknowledgement must mean the pending task future and receiver are gone"
            );
            assert!(!completed.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(
                probe.count(),
                1,
                "abort completion must not re-wake the retired panicking caller"
            );
        });
    }

    #[test]
    fn join_set_abort_all_retains_and_drains_real_task_cancellation() {
        struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let dropped_by_task = std::sync::Arc::clone(&dropped);
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let mut set = task::JoinSet::new();
            set.spawn(async move {
                let _drop_flag = DropFlag(dropped_by_task);
                let _ = started_tx.send(());
                std::future::pending::<()>().await;
            });

            await_test_signal(started_rx, "JoinSet child start").await;

            set.abort_all();
            assert_eq!(set.len(), 1, "abort_all must retain handles for draining");
            let result = set
                .join_next()
                .await
                .expect("retained aborted task acknowledgement");
            assert!(matches!(result, Err(ref error) if error.is_cancelled()));
            assert!(
                dropped.load(std::sync::atomic::Ordering::SeqCst),
                "join acknowledgement must follow task-future destruction"
            );
            assert!(set.is_empty());
        });
    }

    #[test]
    fn join_set_persistent_registration_failure_is_reported_once_and_retained() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let mut set = task::JoinSet::new();
            set.spawn(std::future::pending::<()>());
            set.force_join_registration_failure_for_test();

            let error = set
                .join_next()
                .await
                .expect("set contains one task")
                .expect_err("forced registration failure must be observable");
            assert_eq!(error.kind(), task::JoinErrorKind::WakerRegistrationFailed);
            assert_eq!(
                set.settlement(),
                task::JoinSetSettlement::Incomplete {
                    active_tasks: 0,
                    unacknowledged_tasks: 1,
                }
            );

            assert!(
                set.join_next().await.is_none(),
                "a persistent caller-waker failure must not spin on the same task"
            );
            assert_eq!(set.unacknowledged_len(), 1);

            let drain_cx = crate::cx::Cx::for_testing();
            let terminal = timeout_with_cx(
                &drain_cx,
                Duration::from_secs(1),
                set.drain_next_with_cx(&drain_cx),
            )
            .await
            .expect("trusted drain must remain bounded")
            .expect("trusted drain context must remain live");
            let error = terminal
                .expect("aborted quarantined task must become terminally drainable")
                .expect_err("pending task was aborted after registration failure");
            assert_eq!(error.kind(), task::JoinErrorKind::Aborted);
            assert_eq!(
                set.drain_next_with_cx(&drain_cx).await,
                Ok(None),
                "drain None is reserved for genuine terminal settlement"
            );
            assert_eq!(set.settlement(), task::JoinSetSettlement::Settled);
        });
    }

    #[test]
    fn join_set_unbounded_trusted_drain_settles_quarantined_handle() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let mut set = task::JoinSet::new();
            set.spawn(std::future::pending::<()>());
            set.force_join_registration_failure_for_test();

            let observation_error = set
                .drain_next_trusted()
                .await
                .expect("set contains one task")
                .expect_err("forced caller-waker failure must be observed once");
            assert_eq!(
                observation_error.kind(),
                task::JoinErrorKind::WakerRegistrationFailed
            );
            assert_eq!(
                set.settlement(),
                task::JoinSetSettlement::Incomplete {
                    active_tasks: 0,
                    unacknowledged_tasks: 1,
                },
                "observation failure must enter quarantine before settlement"
            );

            let terminal_error = set
                .drain_next_trusted()
                .await
                .expect("quarantined handle must reach terminal acknowledgement")
                .expect_err("registration failure requested task abort");
            assert_eq!(terminal_error.kind(), task::JoinErrorKind::Aborted);
            assert!(set.drain_next_trusted().await.is_none());
            assert_eq!(set.settlement(), task::JoinSetSettlement::Settled);
        });
    }

    #[test]
    fn join_set_cx_join_quarantines_nonterminal_registration_failure_without_spin() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let mut set = task::JoinSet::new();
            set.spawn(std::future::pending::<()>());
            set.force_join_registration_failure_for_test();
            let cx = crate::cx::Cx::for_testing();

            let error = set
                .join_next_with_cx(&cx)
                .await
                .expect("set contains one task")
                .expect_err("forced registration failure must be observable");
            assert_eq!(error.kind(), task::JoinErrorKind::WakerRegistrationFailed);
            assert!(set.join_next_with_cx(&cx).await.is_none());
            assert_eq!(set.unacknowledged_len(), 1);

            let terminal =
                timeout_with_cx(&cx, Duration::from_secs(1), set.drain_next_with_cx(&cx))
                    .await
                    .expect("trusted drain must remain bounded")
                    .expect("trusted drain context must remain live");
            let error = terminal
                .expect("trusted polling must recover terminal authority")
                .expect_err("pending task was aborted after registration failure");
            assert_eq!(error.kind(), task::JoinErrorKind::Aborted);
            assert!(set.is_empty());
        });
    }

    #[test]
    fn join_set_trusted_drain_repolls_quarantine_while_an_active_task_is_pending() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let mut set = task::JoinSet::new();
            set.spawn(std::future::pending::<()>());
            set.spawn(std::future::pending::<()>());
            set.force_join_registration_failure_for_test();

            let observation_error = set
                .join_next()
                .await
                .expect("set contains two tasks")
                .expect_err("forced registration failure must be observable");
            assert_eq!(
                observation_error.kind(),
                task::JoinErrorKind::WakerRegistrationFailed
            );
            assert_eq!(
                set.settlement(),
                task::JoinSetSettlement::Incomplete {
                    active_tasks: 1,
                    unacknowledged_tasks: 1,
                }
            );

            let drain_cx = crate::cx::Cx::for_testing();
            let quarantined_result = timeout_with_cx(
                &drain_cx,
                Duration::from_secs(1),
                set.drain_next_with_cx(&drain_cx),
            )
            .await
            .expect("mixed active/quarantined trusted drain must remain live")
            .expect("trusted drain context must remain live")
            .expect("quarantined task must reach a terminal result");
            assert!(matches!(
                quarantined_result,
                Err(ref error) if error.kind() == task::JoinErrorKind::Aborted
            ));
            assert_eq!(
                set.settlement(),
                task::JoinSetSettlement::Incomplete {
                    active_tasks: 1,
                    unacknowledged_tasks: 0,
                },
                "the unrelated active task must remain owned"
            );

            set.abort_all();
            let active_result = timeout_with_cx(
                &drain_cx,
                Duration::from_secs(1),
                set.drain_next_with_cx(&drain_cx),
            )
            .await
            .expect("active task abort must remain bounded")
            .expect("trusted drain context must remain live")
            .expect("active task must reach a terminal result");
            assert!(matches!(
                active_result,
                Err(ref error) if error.kind() == task::JoinErrorKind::Aborted
            ));
            assert_eq!(set.settlement(), task::JoinSetSettlement::Settled);
        });
    }

    #[test]
    fn join_set_try_join_next_uses_trusted_terminal_poll_after_registration_failure() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let mut set = task::JoinSet::new();
            let (completed_tx, completed_rx) = oneshot::channel();
            set.spawn(async move {
                let _ = completed_tx.send(());
                7_u32
            });
            await_test_signal(completed_rx, "completed JoinSet child").await;
            task::yield_now().await;
            set.force_join_registration_failure_for_test();

            assert_eq!(
                set.try_join_next()
                    .expect("completed task must become synchronously pollable"),
                Ok(7)
            );
            assert_eq!(set.settlement(), task::JoinSetSettlement::Settled);
        });
    }

    #[test]
    fn task_spawn_blocking_returns_join_handle() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let handle: task::JoinHandle<String> = task::spawn_blocking(|| "hello".to_string());
            let val = handle.await.expect("join");
            assert_eq!(val, "hello");
        });
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn task_panic_becomes_content_free_join_error() {
        async fn panic_at_join_boundary() {
            panic!("task-secret-that-must-not-reach-the-join-error");
        }

        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let result = task::spawn(panic_at_join_boundary()).await;
            let Err(error) = result else {
                panic!("panicking task must fail its JoinHandle");
            };

            assert_eq!(error.to_string(), "JoinError: task failed at join boundary");
            assert!(!error.is_cancelled());
            assert!(!error.to_string().contains("task-secret"));
        });
    }

    // ─── br-ft-iaxog: JoinHandle downstream-waker poison recovery ────
    //
    // Pre-fix every JoinHandle downstream-waker lock-site used
    // `.expect("abort waker mutex poisoned")`. The hot-path site at
    // `JoinHandle::poll` is called by the executor on EVERY task
    // wakeup — a panic in any thread holding the downstream Mutex
    // killed the executor.
    //
    // Post-fix: ContainedForwardingWaker recovers via
    // PoisonError::into_inner() and bumps JOIN_HANDLE_LOCK_POISONED_COUNT.
    //
    // Counter is process-wide; tests serialize via a Mutex test-lock.

    fn join_handle_poison_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn join_handle_lock_poisoned_count_zero_baseline() {
        let _guard = join_handle_poison_test_lock();
        task::reset_join_handle_lock_poisoned_count_for_test();
        assert_eq!(
            task::join_handle_lock_poisoned_count(),
            0,
            "br-ft-iaxog: counter must start at 0 after reset"
        );
    }

    #[test]
    fn join_handle_lock_poisoned_count_unchanged_for_clean_spawn_await() {
        // Negative test: 5 clean spawn+await cycles must NOT bump the
        // counter. Without this assertion the metric would be useless
        // — every task completion would inflate it.
        let _guard = join_handle_poison_test_lock();
        task::reset_join_handle_lock_poisoned_count_for_test();

        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            for i in 0..5 {
                let handle = task::spawn(async move { i * 2 });
                let val = handle.await.expect("join");
                assert_eq!(val, i * 2);
            }
        });

        assert_eq!(
            task::join_handle_lock_poisoned_count(),
            0,
            "br-ft-iaxog: clean spawn+await cycles must NOT bump the counter"
        );
    }

    // The shared forwarding state is exercised by every spawned task and
    // oneshot receive. Its lock never clones, wakes, or drops caller wakers;
    // poison recovery remains the fail-soft defense for an already poisoned
    // slot.

    // ========================================================================
    // join! macro tests
    // ========================================================================

    // ========================================================================
    // select! macro tests
    // ========================================================================

    // ========================================================================
    // task::yield_now tests
    // ========================================================================

    #[test]
    fn yield_now_does_not_panic() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            task::yield_now().await;
        });
    }

    #[test]
    fn yield_now_multiple_times() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            for _ in 0..5 {
                task::yield_now().await;
            }
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `JoinSet::join_next_with_cx` must
    /// short-circuit on entry when the caller's cx is already
    /// cancelled — never polling the inner handles. A
    /// pre-cancelled cx with a never-completing task must yield
    /// `Some(Err(JoinErrorKind::ContextCancelled))` immediately
    /// instead of blocking forever.
    #[test]
    fn join_next_with_cx_short_circuits_on_precancelled_cx() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel join_next"),
            );

            let mut set: task::JoinSet<()> = task::JoinSet::new();
            // Spawn a task that will never complete on its own.
            set.spawn(async {
                std::future::pending::<()>().await;
            });

            let result = set.join_next_with_cx(&cx).await;
            match result {
                Some(Err(err)) => {
                    assert!(
                        err.is_cancelled(),
                        "pre-cancelled cx must produce a cancelled JoinError: {err}"
                    );
                    assert_eq!(err.kind(), task::JoinErrorKind::ContextCancelled);
                    assert!(!err.to_string().contains("pre-cancel join_next"));
                    assert!(!format!("{err:?}").contains("pre-cancel join_next"));
                }
                other => panic!("expected Some(Err(cancelled)) on pre-cancel, got {other:?}"),
            }
        });
    }

    /// ft-xbnl0.2.4 tick 383: `timeout_with_cx` observes cx budget deadline.
    ///
    /// Parallel to tick 382's `sleep_with_cx` budget test. Tick 328's
    /// doc comment on `timeout_with_cx` claimed budget observation but
    /// never tested it. This test pins the claim.
    ///
    /// Setup: cx with `Budget::with_deadline(Time::ZERO)` (budget already
    /// elapsed). Call `timeout_with_cx(cx, 30s, pending_future)` — the
    /// inner future never resolves, so without budget observation the
    /// call would wait the full 30 seconds. With budget observation, it
    /// must return Err promptly.
    #[test]
    fn timeout_with_cx_observes_budget_deadline() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let budget =
                asupersync::types::Budget::new().with_deadline(asupersync::types::Time::ZERO);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);

            let started = std::time::Instant::now();
            let result: std::result::Result<(), String> =
                timeout_with_cx(&cx, Duration::from_secs(30), std::future::pending::<()>()).await;
            let elapsed = started.elapsed();

            assert!(
                result.is_err(),
                "expired-budget cx must cause timeout_with_cx to return Err"
            );
            assert!(
                elapsed < Duration::from_secs(5),
                "expired-budget cx must cause timeout_with_cx to return promptly; \
                 took {elapsed:?}"
            );
        });
    }

    #[test]
    fn typed_timeout_preserves_elapsed_failure_class() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let budget =
                asupersync::types::Budget::new().with_deadline(asupersync::types::Time::ZERO);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);

            let error =
                timeout_with_cx_typed(&cx, Duration::from_secs(30), std::future::pending::<()>())
                    .await
                    .expect_err("expired budget must terminate the typed timeout");

            assert_eq!(error, TimeoutError::Elapsed);
        });
    }

    /// ft-xbnl0.2.4 tick 382: `sleep_with_cx` observes cx budget deadline.
    ///
    /// The tick-331 doc comment on `sleep_with_cx` states the primitive
    /// observes the cx **budget deadline** (via
    /// `asupersync::time::budget_sleep`, which caps effective sleep by
    /// remaining budget) even though it does NOT observe direct
    /// `is_cancel_requested()`. This test pins that claim.
    ///
    /// Setup: create a cx whose budget has already elapsed
    /// (`Time::ZERO` deadline), then call `sleep_with_cx(cx,
    /// Duration::from_secs(30))`. If budget observation works, the
    /// sleep returns fast (Err) instead of blocking for 30 seconds.
    ///
    /// Observable contract:
    /// 1. Return type is Err (budget exceeded).
    /// 2. Elapsed << 30s (well under the requested duration).
    ///
    /// This complements the tick-378 `distributed_http_client_...`
    /// snapshot: that pins budget-aware behavior at the HTTP client
    /// layer (transitively via asupersync's HTTP client); this pins
    /// it at the foundational runtime_async primitive level.
    #[test]
    fn sleep_with_cx_observes_budget_deadline() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Budget with deadline already elapsed.
            let budget =
                asupersync::types::Budget::new().with_deadline(asupersync::types::Time::ZERO);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);

            let started = std::time::Instant::now();
            let result = sleep_with_cx(&cx, Duration::from_secs(30)).await;
            let elapsed = started.elapsed();

            assert!(
                result.is_err(),
                "expired-budget cx must cause sleep_with_cx to return Err, got: {result:?}"
            );
            assert!(
                elapsed < Duration::from_secs(5),
                "expired-budget cx must cause sleep_with_cx to return promptly, \
                 not block for the requested duration; took {elapsed:?}"
            );
        });
    }

    /// ft-xbnl0.2.4 tick 418: `yield_now_with_cx` observes cx-cancel
    /// via the `cx.checkpoint()` pre-guard.
    ///
    /// Unlike `sleep_with_cx` / `timeout_with_cx` (which observe the
    /// budget deadline but NOT direct `is_cancel_requested()`),
    /// `yield_now_with_cx` **does** observe direct cx-cancel because
    /// its implementation calls `cx.checkpoint()?` before yielding.
    /// Tight poll loops use this as their cancellation-sensing yield
    /// point — the return-`Err` contract is how they break cleanly
    /// instead of spinning forever against a cancelled cx.
    ///
    /// This pins that contract. Setup: pre-cancelled cx (no budget
    /// manipulation — cancel is direct). `yield_now_with_cx(&cx)`
    /// must return a typed cancellation rather than yielding.
    ///
    /// Complements the two budget-observation tests above: together
    /// the three tests cover the matrix of primitive × signal-kind:
    /// - `sleep_with_cx`: budget ✓, direct-cancel ✗
    /// - `timeout_with_cx`: budget ✓, direct-cancel ✗
    /// - `yield_now_with_cx`: direct-cancel ✓ (this tick)
    #[test]
    fn yield_now_with_cx_observes_cx_cancel_checkpoint() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("SECRET tick 418 pre-cancel yield test"),
            );

            let started = std::time::Instant::now();
            let result = task::yield_now_with_cx(&cx).await;
            let elapsed = started.elapsed();

            assert!(
                result.is_err(),
                "pre-cancelled cx must cause yield_now_with_cx to return Err, got: {result:?}"
            );
            let error = result.expect_err("pre-cancel must fail");
            assert_eq!(error.kind(), task::JoinErrorKind::ContextCancelled);
            assert!(!error.to_string().contains("SECRET"));
            assert!(!format!("{error:?}").contains("SECRET"));
            assert!(
                elapsed < Duration::from_secs(1),
                "pre-cancelled cx must short-circuit yield_now_with_cx promptly; took {elapsed:?}"
            );
        });
    }

    #[test]
    fn join_error_context_failure_mapping_is_exhaustive_and_content_free() {
        use crate::outcome::CancelKind;

        let cases = [
            (
                CancelKind::User,
                task::JoinErrorKind::ContextCancelled,
                true,
            ),
            (
                CancelKind::Timeout,
                task::JoinErrorKind::DeadlineExceeded,
                false,
            ),
            (
                CancelKind::Deadline,
                task::JoinErrorKind::DeadlineExceeded,
                false,
            ),
            (
                CancelKind::PollQuota,
                task::JoinErrorKind::PollQuotaExhausted,
                false,
            ),
            (
                CancelKind::CostBudget,
                task::JoinErrorKind::CostBudgetExhausted,
                false,
            ),
            (
                CancelKind::FailFast,
                task::JoinErrorKind::ContextCancelled,
                true,
            ),
            (
                CancelKind::RaceLost,
                task::JoinErrorKind::ContextCancelled,
                true,
            ),
            (
                CancelKind::ParentCancelled,
                task::JoinErrorKind::ContextCancelled,
                true,
            ),
            (
                CancelKind::ResourceUnavailable,
                task::JoinErrorKind::ContextCancelled,
                true,
            ),
            (
                CancelKind::Shutdown,
                task::JoinErrorKind::ContextCancelled,
                true,
            ),
            (
                CancelKind::LinkedExit,
                task::JoinErrorKind::ContextCancelled,
                true,
            ),
        ];

        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for (kind, expected, expected_cancelled) in cases {
                let cx = crate::cx::Cx::for_testing();
                cx.cancel_with(kind, Some("SECRET exhaustive join classification"));
                let error = task::yield_now_with_cx(&cx)
                    .await
                    .expect_err("cancelled context must fail the Cx-aware yield");
                assert_eq!(error.kind(), expected);
                assert_eq!(error.is_cancelled(), expected_cancelled);
                assert!(!error.to_string().contains("SECRET"));
                assert!(!format!("{error:?}").contains("SECRET"));
            }
        });
    }

    /// ft-xbnl0.2.4 tick 418: `yield_now_with_cx` happy path — a live
    /// (uncancelled) cx must allow the primitive to yield once and
    /// return Ok.
    ///
    /// Pair with `yield_now_with_cx_observes_cx_cancel_checkpoint`:
    /// together they pin both branches of the `cx.checkpoint()?`
    /// pre-guard.
    #[test]
    fn yield_now_with_cx_yields_on_live_cx() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cx = crate::cx::for_request();

            let started = std::time::Instant::now();
            let result = task::yield_now_with_cx(&cx).await;
            let elapsed = started.elapsed();

            assert!(
                result.is_ok(),
                "live cx must allow yield_now_with_cx to succeed, got: {result:?}"
            );
            assert!(
                elapsed < Duration::from_secs(1),
                "yield_now_with_cx must return promptly on live cx; took {elapsed:?}"
            );
        });
    }

    /// ft-xbnl0.2.4 tick 426: `JoinSet::join_next_with_cx` observes cx-cancel.
    ///
    /// Pins cx-cancel observation on the runtime_async JoinSet
    /// primitive. With a spawned task that never completes
    /// (`pending::<()>().await`) and a pre-cancelled cx,
    /// `set.join_next_with_cx(&cx).await` must return
    /// `Some(Err(JoinErrorKind::ContextCancelled))` rather than blocking
    /// indefinitely for a task that will never complete.
    ///
    /// Cancel semantics surfaced by runtime_async's own pre-flight:
    ///
    ///     if caller_cx.checkpoint().is_err() {
    ///         return Some(Err(JoinError::from_context_failure(caller_cx)));
    ///     }
    ///
    /// Unlike the channel/semaphore primitives which delegate their
    /// cx-cancel observability to asupersync's own `poll_*`
    /// short-circuit, `JoinSet::join_next_with_cx` is a
    /// runtime_async-owned primitive (it wraps a local Vec<JoinHandle>
    /// rather than an asupersync primitive). The test guards against
    /// regressions in the runtime_async-level pre-flight +
    /// per-poll-iteration checkpoint.
    ///
    /// Setup:
    /// 1. Create a JoinSet.
    /// 2. Spawn a task that blocks forever via `pending::<()>().await`.
    /// 3. Pre-cancel cx via `cx.cancel_with(User, ...)`.
    /// 4. Wrap `set.join_next_with_cx(&cx)` in a 2 s outer safety-net
    ///    timeout.
    /// 5. Assert elapsed < 1 s AND a typed cancelled JoinError that does not
    ///    expose the cancellation reason.
    ///
    /// `join_next_with_cx` also surfaces `JoinError::is_cancelled()` as true
    /// from the structural kind, with no message parsing.
    #[test]
    fn join_set_join_next_with_cx_observes_pre_cancel() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut set = task::JoinSet::<()>::new();
            set.spawn(async {
                std::future::pending::<()>().await;
            });

            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("SECRET tick 426 pre-cancel JoinSet test"),
            );

            let started = std::time::Instant::now();
            let result = timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(2),
                set.join_next_with_cx(&cx),
            )
            .await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "pre-cancelled cx must short-circuit JoinSet::join_next_with_cx promptly; \
                 took {elapsed:?} (outer 2s timeout likely fired)"
            );
            let inner = result.expect("outer timeout must not fire with cx-cancel observation");
            match inner {
                Some(Err(err)) => {
                    let msg = err.to_string();
                    assert_eq!(err.kind(), task::JoinErrorKind::ContextCancelled);
                    assert!(
                        err.is_cancelled(),
                        "pre-cancelled cx must make JoinError::is_cancelled() return true; \
                         msg: {msg}"
                    );
                    assert!(!msg.contains("SECRET"));
                    assert!(!format!("{err:?}").contains("SECRET"));
                }
                Some(Ok(())) => panic!(
                    "pre-cancelled cx must surface Err(JoinError), not Ok — pending task \
                     cannot complete"
                ),
                None => panic!(
                    "pre-cancelled cx must surface Some(Err(JoinError)), not None — set \
                     has one pending task"
                ),
            }
        });
    }

    /// ft-xbnl0.2.4 tick 423: `watch::Receiver::changed(cx)` observes cx-cancel.
    ///
    /// Pins cx-cancel observation on the asupersync watch-channel
    /// `changed` primitive. With the sender alive but no new value
    /// published (version unchanged) and a pre-cancelled cx,
    /// `rx.changed(&cx).await` must return `Err(RecvError::Cancelled)`
    /// promptly rather than blocking for a publish that is not coming.
    ///
    /// Cancel semantics surfaced by asupersync's `poll_changed`:
    ///
    ///     if cx.checkpoint().is_err() {
    ///         Poll::Ready(Err(RecvError::Cancelled))
    ///     }
    ///
    /// Setup:
    /// 1. Create `(tx, mut rx) = watch::channel::<u64>(0)` — keep `_tx` alive.
    /// 2. Pre-cancel cx via `cx.cancel_with(User, ...)`.
    /// 3. Wrap `rx.changed(&cx)` in a 2 s outer safety-net timeout.
    /// 4. Assert elapsed < 1 s AND `Err(RecvError::Cancelled)`.
    ///
    /// The project-owned watch wrapper delegates this checkpoint to
    /// asupersync while retaining only its stable trusted waker proxy. Watch
    /// channels underpin config-change notification and snapshot-version
    /// signalling — pinning cx-cancel guards those sites.
    #[test]
    fn watch_changed_with_cx_observes_pre_cancel() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_tx, mut rx) = watch::channel::<u64>(0);
            // Mark the initial value as seen so `changed` waits for a
            // new publish rather than returning immediately.
            let _ = rx.borrow_and_update();

            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("tick 423 pre-cancel watch::Receiver::changed test"),
            );

            let started = std::time::Instant::now();
            let result = timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(2),
                rx.changed(&cx),
            )
            .await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "pre-cancelled cx must short-circuit watch::Receiver::changed promptly; \
                 took {elapsed:?} (outer 2s timeout likely fired)"
            );
            let inner = result.expect("outer timeout must not fire with cx-cancel observation");
            assert!(
                matches!(inner, Err(watch::RecvError::Cancelled)),
                "pre-cancelled cx must yield watch::RecvError::Cancelled (not Closed); \
                 got: {inner:?}"
            );
        });
    }

    /// ft-xbnl0.2.4 tick 439a: extends the tick 432-438 probes to
    /// `Semaphore::acquire_with_cx`. Fully contended semaphore
    /// (zero permits), spawn cancel at 100 ms, select-race against
    /// poll-sleep watcher. Tolerates both Either outcomes.
    ///
    /// **Outcome when written**: watcher branch fires consistently,
    /// confirming Semaphore::acquire_with_cx shares the mid-flight-
    /// cancel-waker gap with the four channel types. Together with
    /// tick 439b (JoinSet) this completes the mid-flight matrix
    /// across all six long-lived-wait primitives in runtime_async.
    #[test]
    fn semaphore_acquire_with_cx_mid_flight_cancel_via_select_race_pattern() {
        use futures::future::Either;
        use futures::future::select;
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(0);
            let cx = crate::cx::Cx::for_testing();
            let cancel_trigger = cx.clone();

            task::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                cancel_trigger.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("tick 439a mid-flight cancel via semaphore select race pattern"),
                );
            });

            let acquire_fut = std::pin::pin!(async { sem.acquire_with_cx(&cx).await.map(|_| ()) });
            let watcher = std::pin::pin!(async {
                loop {
                    sleep(Duration::from_millis(50)).await;
                    if cx.is_cancel_requested() {
                        return Err::<(), String>("cancelled via watcher".to_string());
                    }
                }
            });

            let started = std::time::Instant::now();
            let outcome = select(acquire_fut, watcher).await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "select! race watcher must catch mid-flight cancel within ~1 s \
                 (expected ~150 ms; 1000 ms envelope absorbs load drift); took {elapsed:?}"
            );
            match outcome {
                Either::Right((Err(_), _)) => {}
                Either::Left((Err(_), _)) => {}
                Either::Left((Ok(()), _)) => {
                    panic!("acquire branch returned Ok — semaphore had no permits to grant")
                }
                Either::Right((Ok(()), _)) => {
                    unreachable!("watcher always returns Err on cancel")
                }
            }
        });
    }

    /// ft-xbnl0.2.4 tick 439b: extends the tick 432-438 probes to
    /// `JoinSet::join_next_with_cx`. Spawned-but-pending task,
    /// spawn cancel at 100 ms, select-race against poll-sleep watcher.
    ///
    /// JoinSet::join_next_with_cx has TWO cancel-observability points
    /// (tick 426): a pre-flight `caller_cx.checkpoint()` AND a
    /// per-poll-iteration `caller_cx.checkpoint()` inside the
    /// `poll_fn` closure. This means the task CAN observe mid-flight
    /// cancel — the per-poll checkpoint fires on each re-poll
    /// triggered by task completion or external wake, surfacing
    /// `Some(Err(JoinError))`.
    ///
    /// **Outcome when written**: unlike the four channel types,
    /// EITHER branch can fire — if an external wake happens during
    /// the cancel window, the recv-side runtime_async loop re-polls,
    /// sees `caller_cx.checkpoint().is_err()`, and returns
    /// `Some(Err(JoinError))`. If no external wake happens, the
    /// watcher branch catches it. Both outcomes are acceptable;
    /// both converge on fast observation.
    #[test]
    fn join_set_join_next_with_cx_mid_flight_cancel_via_select_race_pattern() {
        use futures::future::Either;
        use futures::future::select;
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut set = task::JoinSet::<()>::new();
            set.spawn(async {
                std::future::pending::<()>().await;
            });

            let cx = crate::cx::Cx::for_testing();
            let cancel_trigger = cx.clone();

            task::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                cancel_trigger.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("tick 439b mid-flight cancel via JoinSet select race pattern"),
                );
            });

            let join_fut = std::pin::pin!(async { set.join_next_with_cx(&cx).await });
            let watcher = std::pin::pin!(async {
                loop {
                    sleep(Duration::from_millis(50)).await;
                    if cx.is_cancel_requested() {
                        return Err::<(), String>("cancelled via watcher".to_string());
                    }
                }
            });

            let started = std::time::Instant::now();
            let outcome = select(join_fut, watcher).await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "select! race must catch mid-flight cancel within ~1 s \
                 (expected ~150 ms; 1000 ms envelope absorbs load drift); took {elapsed:?}"
            );
            match outcome {
                Either::Right((Err(_), _)) => {}
                Either::Left((Some(Err(_)), _)) => {}
                Either::Left((None, _)) => {
                    panic!("join_next returned None — set had one pending task")
                }
                Either::Left((Some(Ok(())), _)) => {
                    panic!("join_next returned Ok — pending task cannot complete")
                }
                Either::Right((Ok(()), _)) => {
                    unreachable!("watcher always returns Err on cancel")
                }
            }
        });
    }

    /// ft-xbnl0.2.4 tick 438: extends the tick 432/433/434 channel
    /// probes to `watch::Receiver::changed`. Same shape: cancel-at-100-ms,
    /// select-race, tolerate both Either outcomes.
    ///
    /// **Outcome when written**: watcher branch fires consistently,
    /// confirming watch shares the same mid-flight-cancel-waker gap
    /// as mpsc/oneshot/broadcast. The tick-437 watch module doc
    /// comment's "likely present based on shared design" can now be
    /// upgraded to "confirmed" — all four asupersync channel types
    /// observe pre-cancel via per-poll checkpoint but do NOT
    /// register cx-cancel-wakers for already-suspended recvs.
    #[test]
    fn watch_changed_with_cx_mid_flight_cancel_via_select_race_pattern() {
        use futures::future::Either;
        use futures::future::select;
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_tx, mut rx) = watch::channel::<u64>(0);
            let _ = rx.borrow_and_update();
            let cx = crate::cx::Cx::for_testing();
            let cancel_trigger = cx.clone();

            task::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                cancel_trigger.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("tick 438 mid-flight cancel via watch select race pattern"),
                );
            });

            let recv_fut = std::pin::pin!(async { rx.changed(&cx).await });
            let watcher = std::pin::pin!(async {
                loop {
                    sleep(Duration::from_millis(50)).await;
                    if cx.is_cancel_requested() {
                        return Err::<(), String>("cancelled via watcher".to_string());
                    }
                }
            });

            let started = std::time::Instant::now();
            let outcome = select(recv_fut, watcher).await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "select! race watcher must catch mid-flight cancel within ~1 s \
                 (expected ~150 ms; 1000 ms envelope absorbs load drift); took {elapsed:?}"
            );
            match outcome {
                Either::Right((Err(_), _)) => {
                    // Expected: watcher branch fired with cancel.
                }
                Either::Left((Err(_), _)) => {
                    // Acceptable: asupersync gained waker support.
                }
                Either::Left((Ok(()), _)) => {
                    panic!("recv branch returned Ok — no new value was published")
                }
                Either::Right((Ok(()), _)) => {
                    unreachable!("watcher always returns Err on cancel")
                }
            }
        });
    }

    /// ft-xbnl0.2.4 tick 434: extends the tick 432/433 probe to
    /// `broadcast_recv_with_cx`. Same shape as the mpsc / oneshot
    /// mid-flight tests: cancel-at-100-ms task, select-race against
    /// a poll-sleep watcher, tolerate both Either outcomes.
    ///
    /// **Outcome when written**: watcher branch fires consistently,
    /// confirming the same gap applies to broadcast_recv_with_cx —
    /// asupersync broadcast receivers do not register cx-cancel-wakers
    /// for already-suspended recvs. Together with ticks 432/433, this
    /// generalises the finding to the three most-used channel recv
    /// primitives across the core.
    ///
    /// Broadcast is especially impacted: it underpins the event fanout
    /// layer (`events.rs`, `ipc.rs`), so cx-cancel mid-flight on a
    /// broadcast subscriber wouldn't be observed without the
    /// select-race pattern.
    #[test]
    fn broadcast_recv_with_cx_mid_flight_cancel_via_select_race_pattern() {
        use futures::future::Either;
        use futures::future::select;
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_tx, _rx_keepalive) = broadcast::channel::<u64>(4);
            let mut rx = _tx.subscribe();
            let cx = crate::cx::Cx::for_testing();
            let cancel_trigger = cx.clone();

            task::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                cancel_trigger.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("tick 434 mid-flight cancel via broadcast select race pattern"),
                );
            });

            let recv_fut =
                std::pin::pin!(async { broadcast_recv_with_cx(&cx, &mut rx).await.map(|_| ()) });
            let watcher = std::pin::pin!(async {
                loop {
                    sleep(Duration::from_millis(50)).await;
                    if cx.is_cancel_requested() {
                        return Err::<(), String>("cancelled via watcher".to_string());
                    }
                }
            });

            let started = std::time::Instant::now();
            let outcome = select(recv_fut, watcher).await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "select! race watcher must catch mid-flight cancel within ~1 s \
                 (expected ~150 ms; 1000 ms envelope absorbs load drift); took {elapsed:?}"
            );
            match outcome {
                Either::Right((Err(_), _)) => {
                    // Expected: watcher branch fired with cancel.
                }
                Either::Left((Err(_), _)) => {
                    // Acceptable: asupersync gained waker support.
                }
                Either::Left((Ok(()), _)) => {
                    panic!("recv branch returned Ok — sender should not have published")
                }
                Either::Right((Ok(()), _)) => {
                    unreachable!("watcher always returns Err on cancel")
                }
            }
        });
    }

    /// ft-xbnl0.2.4 tick 433: probes whether the tick-432 mpsc
    /// mid-flight-cancel-waker gap applies to `oneshot_recv_with_cx`
    /// as well. Pins the result as a contract test.
    ///
    /// Same shape as tick 432: create channel, spawn task that fires
    /// `cx.cancel_with` at 100 ms, race the recv against a poll-sleep
    /// watcher. If oneshot DOES register a cx-cancel-waker, the
    /// `Either::Left(Err)` recv branch fires first; if it does NOT
    /// (same gap as mpsc), the `Either::Right` watcher branch fires.
    ///
    /// Empirically captures the primitive's mid-flight waker behavior
    /// so a regression in either direction fires a test.
    ///
    /// **Outcome when written**: watcher branch fires consistently,
    /// confirming the same gap applies to oneshot_recv_with_cx as to
    /// mpsc. Callers needing mid-flight cancel on oneshot must use
    /// the same `futures::future::select` race pattern.
    #[test]
    fn oneshot_recv_with_cx_mid_flight_cancel_via_select_race_pattern() {
        use futures::future::Either;
        use futures::future::select;
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_tx, rx) = oneshot::channel::<u64>();
            let cx = crate::cx::Cx::for_testing();
            let cancel_trigger = cx.clone();

            task::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                cancel_trigger.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("tick 433 mid-flight cancel via oneshot select race pattern"),
                );
            });

            let recv_fut =
                std::pin::pin!(async { oneshot_recv_with_cx(&cx, rx).await.map(|_| ()) });
            let watcher = std::pin::pin!(async {
                loop {
                    sleep(Duration::from_millis(50)).await;
                    if cx.is_cancel_requested() {
                        return Err::<(), String>("cancelled via watcher".to_string());
                    }
                }
            });

            let started = std::time::Instant::now();
            let outcome = select(recv_fut, watcher).await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "select! race watcher must catch mid-flight cancel within ~1 s \
                 (expected ~150 ms; 1000 ms envelope absorbs load drift); took {elapsed:?}"
            );
            match outcome {
                Either::Right((Err(_), _)) => {
                    // Expected: watcher branch fired with cancel
                    // (same gap as mpsc — no cx-cancel-waker).
                }
                Either::Left((Err(_), _)) => {
                    // Acceptable: oneshot itself observed the cancel
                    // (would mean asupersync gained waker support —
                    // improvement, not regression).
                }
                Either::Left((Ok(()), _)) => {
                    panic!("recv branch returned Ok — sender should not have fired")
                }
                Either::Right((Ok(()), _)) => {
                    unreachable!("watcher always returns Err on cancel")
                }
            }
        });
    }

    /// ft-xbnl0.2.4 tick 432: documents and pins the caller-side
    /// mid-flight cx-cancel pattern for `mpsc::Receiver::recv` —
    /// `select!` race against a poll-based cx-cancel watcher.
    ///
    /// **Finding**: asupersync `mpsc::Receiver::recv(cx)` observes
    /// pre-cancel via its per-poll `cx.checkpoint().is_err()`
    /// short-circuit (pinned by tick 422) but does NOT register a
    /// cx-cancel-waker. A recv that has already suspended on the
    /// send-side waker will not wake when `cx.cancel_with(...)` fires
    /// after the suspension. The test-worthy mid-flight cancel
    /// pattern is therefore the same caller-side workaround that
    /// `DistributedHttpClient` uses (tick 387 `race_with_cx_cancel`):
    /// wrap the recv in a `select!` against a poll-sleep cancel
    /// watcher.
    ///
    /// This test pins that pattern works, giving callers a template.
    /// It also documents the underlying gap: if asupersync ever
    /// registers cx-cancel-wakers on the recv path, the polling
    /// watcher becomes redundant but remains correct.
    ///
    /// Shape:
    /// - Create `(tx, mut rx) = mpsc::channel(4)` — keep `_tx` alive.
    /// - Spawn task that cancels cx after 100 ms.
    /// - Wrap `rx.recv(&cx)` in a `select!` against a poll-sleep
    ///   watcher that returns `Err("cancelled")` when `cx.is_cancel_requested()`.
    /// - Assert elapsed < 500 ms (the poll-sleep watcher catches the
    ///   cancel within its 50 ms poll interval) AND Err.
    #[test]
    fn mpsc_recv_with_cx_mid_flight_cancel_via_select_race_pattern() {
        use futures::future::Either;
        use futures::future::select;
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_tx, mut rx) = mpsc::channel::<u64>(4);
            let cx = crate::cx::Cx::for_testing();
            let cancel_trigger = cx.clone();

            task::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                cancel_trigger.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("tick 432 mid-flight cancel via select race pattern"),
                );
            });

            let recv_fut = std::pin::pin!(async { rx.recv(&cx).await.map(|_| ()) });
            let watcher = std::pin::pin!(async {
                loop {
                    sleep(Duration::from_millis(50)).await;
                    if cx.is_cancel_requested() {
                        return Err("cancelled via watcher");
                    }
                }
            });

            let started = std::time::Instant::now();
            let outcome = select(recv_fut, watcher).await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "select! race watcher must catch mid-flight cancel within ~1 s \
                 (expected ~150 ms: 100 ms cancel trigger + up to 50 ms poll; \
                 1000 ms envelope absorbs concurrent-agent load drift); took {elapsed:?}"
            );
            match outcome {
                Either::Right((Err(_), _recv_still_pending)) => {
                    // Expected: watcher branch fired with cancel.
                }
                Either::Left((Ok(()), _)) => {
                    panic!("recv branch returned Ok — sender should not have fired")
                }
                Either::Left((Err(err), _)) => {
                    // Acceptable alternative: recv itself observed the cancel
                    // (would mean asupersync gained cx-cancel-waker support).
                    assert!(
                        matches!(err, mpsc::RecvError::Cancelled),
                        "if recv branch fires, it must be Cancelled; got: {err:?}"
                    );
                }
                Either::Right((Ok(()), _)) => {
                    unreachable!("watcher returns Err on cancel; Ok is not producible")
                }
            }
        });
    }

    /// ft-xbnl0.2.4 tick 422: `mpsc::Receiver::recv(cx)` observes cx-cancel.
    ///
    /// Pins cx-cancel observation on the asupersync mpsc receiver — the
    /// final core channel primitive in the cancel-matrix. With live
    /// senders (no disconnect), empty channel, and a pre-cancelled cx,
    /// `rx.recv(&cx).await` must return `Err(RecvError::Cancelled)`
    /// promptly.
    ///
    /// The project-owned runtime_async MPSC wrapper delegates cancellation
    /// checks to asupersync while ensuring that the inner primitive retains
    /// only a stable trusted waker proxy. The cancel semantics are surfaced
    /// by asupersync's `poll_recv` short-circuit:
    /// `if cx.checkpoint().is_err() { Poll::Ready(Err(Cancelled)) }`.
    ///
    /// Setup:
    /// 1. Create `(tx, rx) = mpsc::channel(4)` — keep `_tx` alive.
    /// 2. Pre-cancel cx via `cx.cancel_with(User, ...)`.
    /// 3. Wrap `rx.recv(&cx)` in a 2 s outer safety-net timeout.
    /// 4. Assert elapsed < 1 s AND `Err(RecvError::Cancelled)`.
    ///
    /// mpsc is heavily used across `ipc.rs` for shutdown signals and
    /// subscription plumbing — pinning cx-cancel guards those patterns.
    #[test]
    fn mpsc_recv_with_cx_observes_pre_cancel() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_tx, mut rx) = mpsc::channel::<u64>(4);
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("tick 422 pre-cancel mpsc::Receiver::recv test"),
            );

            let started = std::time::Instant::now();
            let result = timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(2),
                rx.recv(&cx),
            )
            .await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "pre-cancelled cx must short-circuit mpsc::Receiver::recv promptly; \
                 took {elapsed:?} (outer 2s timeout likely fired)"
            );
            let inner = result.expect("outer timeout must not fire with cx-cancel observation");
            assert!(
                matches!(inner, Err(mpsc::RecvError::Cancelled)),
                "pre-cancelled cx must yield RecvError::Cancelled (not Disconnected / Empty); \
                 got: {inner:?}"
            );
        });
    }

    /// ft-xbnl0.2.4 tick 430: `Command::output_with_cx` observes pre-cancel
    /// at the pre-spawn `cx.checkpoint()` gate.
    ///
    /// `output_with_cx` has two cancel-observability points:
    /// 1. Pre-spawn `cx.checkpoint()` → `Err(ErrorKind::Interrupted)`
    ///    with message "process command cancelled pre-spawn: ..." before
    ///    any child process is forked.
    /// 2. Mid-flight via a spawned cx→`Arc<AtomicBool>` watcher bridging
    ///    caller-cx cancellation into the `run_output_command` worker's
    ///    cancel flag (polled at `PROCESS_POLL_INTERVAL`).
    ///
    /// This test pins the pre-spawn gate: pre-cancelled cx must cause
    /// `output_with_cx` to return Err BEFORE any child process is
    /// created. That's the strongest guarantee — the caller-side cancel
    /// prevents process-resource leakage, not just fast exit from a
    /// running child.
    ///
    /// Setup:
    /// 1. Build `Command::new("sleep")` with arg `"30"` (never spawned —
    ///    pre-flight gates before fork).
    /// 2. Pre-cancel cx via `cx.cancel_with(User, ...)`.
    /// 3. Wrap `cmd.output_with_cx(&cx)` in 2 s outer safety-net.
    /// 4. Assert elapsed < 500 ms AND Err with
    ///    `ErrorKind::Interrupted` and message containing "pre-spawn".
    ///
    /// Used across the core for running external tools (ft status,
    /// pgrep, sysctl on macOS) under cx control — pinning the pre-spawn
    /// gate guards against regressions where a cancelled cx still
    /// spawns and then immediately kills the child (observable as
    /// wasted forks, zombie handles, or audit-log churn).
    #[cfg(all(feature = "asupersync-runtime", unix))]
    #[test]
    fn command_output_with_cx_observes_pre_cancel_pre_spawn() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut cmd = process::Command::new("sleep");
            cmd.arg("30");

            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("tick 430 pre-cancel Command::output_with_cx test"),
            );

            let started = std::time::Instant::now();
            let result = timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(2),
                cmd.output_with_cx(&cx),
            )
            .await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_millis(500),
                "pre-cancelled cx must short-circuit Command::output_with_cx pre-spawn; \
                 took {elapsed:?} (outer 2s timeout likely fired, or a child was spawned \
                 and then killed which would be a worse-case regression)"
            );
            let inner = result.expect("outer timeout must not fire");
            match inner {
                Err(err) => {
                    assert_eq!(
                        err.kind(),
                        std::io::ErrorKind::Interrupted,
                        "pre-cancel must fold into io::ErrorKind::Interrupted; got {:?}",
                        err.kind()
                    );
                    let msg = err.to_string();
                    assert!(
                        msg.contains("pre-spawn"),
                        "error message should surface the pre-spawn gate specifically \
                         (distinct from mid-flight cancel); got: {msg}"
                    );
                }
                Ok(output) => panic!(
                    "pre-cancelled cx must surface Err, not Ok({output:?}) — a spawned \
                     child process for a cancelled cx would be a regression"
                ),
            }
        });
    }

    /// ft-xbnl0.2.4 tick 429: `unix::next_line_with_cx` observes pre-cancel
    /// via its `cx.checkpoint()` pre-flight.
    ///
    /// `next_line_with_cx` is a seam-level cx-first primitive: it gates
    /// entry to the underlying `lines.next()` wait with a
    /// `cx.checkpoint()` folded into `io::ErrorKind::Interrupted`.
    /// The underlying asupersync stream does NOT itself observe cx on
    /// each poll — the pre-flight is the sole cancel-observability
    /// point. This test pins that pre-flight:
    ///
    ///     cx.checkpoint().map_err(|err| io::Error::new(
    ///         io::ErrorKind::Interrupted,
    ///         format!("next_line cancelled: {err}"),
    ///     ))?;
    ///
    /// Setup:
    /// 1. Bind a UnixListener at a tempdir socket path.
    /// 2. Spawn a task that accepts and holds the connection open
    ///    without writing (reader will pend forever on new data).
    /// 3. Connect a client UnixStream, wrap in BufReader + lines().
    /// 4. Pre-cancel cx via `cx.cancel_with(User, ...)`.
    /// 5. Wrap `next_line_with_cx(&cx, &mut lines)` in 2 s outer
    ///    safety-net timeout.
    /// 6. Assert elapsed < 1 s AND Err with ErrorKind::Interrupted and
    ///    message containing "next_line cancelled".
    ///
    /// `next_line_with_cx` is used across `ipc.rs` for shutdown-aware
    /// line-reading loops — pinning cx-cancel guards those patterns
    /// against regressions in the seam.
    #[cfg(all(feature = "asupersync-runtime", unix))]
    #[test]
    fn unix_next_line_with_cx_observes_pre_cancel() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let tempdir = tempfile::tempdir().expect("tempdir");
            let socket_path = tempdir.path().join("tick429.sock");
            let listener = unix::bind(&socket_path).await.expect("bind");

            // Accept and hold the connection open, never writing.
            // 1 s hold is plenty for the sub-10-ms cancel assertion.
            let _accept_task = task::spawn(async move {
                let _held = listener.accept().await;
                sleep(Duration::from_secs(1)).await;
                drop(_held);
            });

            let client = unix::connect(&socket_path).await.expect("connect");
            let reader = unix::buffered(client);
            let mut lines = unix::lines(reader);

            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("tick 429 pre-cancel unix::next_line_with_cx test"),
            );

            let started = std::time::Instant::now();
            let result = timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(2),
                unix::next_line_with_cx(&cx, &mut lines),
            )
            .await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "pre-cancelled cx must short-circuit unix::next_line_with_cx promptly; \
                 took {elapsed:?} (outer 2s timeout likely fired)"
            );
            let inner = result.expect("outer timeout must not fire");
            match inner {
                Err(err) => {
                    assert_eq!(
                        err.kind(),
                        std::io::ErrorKind::Interrupted,
                        "pre-cancel must fold into io::ErrorKind::Interrupted; got {:?}",
                        err.kind()
                    );
                    let msg = err.to_string();
                    assert!(
                        msg.contains("next_line cancelled"),
                        "error message should surface seam-level cancellation; got: {msg}"
                    );
                }
                Ok(val) => panic!(
                    "pre-cancelled cx must surface Err, not Ok({val:?}) — reader has no data"
                ),
            }

            // _accept_task will be aborted when the runtime drops.
            drop(lines);
        });
    }

    /// ft-xbnl0.2.4 tick 427: `Semaphore::acquire_owned_with_cx` observes cx-cancel.
    ///
    /// Companion to tick 421's borrow-variant test. The owned variant
    /// (used by call sites that need to pass a permit across an await
    /// boundary or into a spawned task) takes `self: Arc<Self>` and
    /// returns `OwnedSemaphorePermit` instead of `SemaphorePermit<'_>`.
    /// The cancel contract is the same: pre-cancelled cx on a
    /// zero-permit semaphore must yield `Err(AcquireError::Cancelled)`
    /// promptly, not block indefinitely.
    ///
    /// Setup (mirrors tick 421):
    /// 1. Construct `Arc::new(Semaphore::new(0))` — fully contended.
    /// 2. Pre-cancel cx via `cx.cancel_with(User, ...)`.
    /// 3. Wrap `sem.clone().acquire_owned_with_cx(&cx)` in a 2 s outer
    ///    safety-net timeout.
    /// 4. Assert elapsed < 1 s AND `Err(AcquireError::Cancelled)`
    ///    specifically.
    ///
    /// Pinning both variants together guards against a regression
    /// where only one path picks up a future cx-cancel-observability
    /// change in asupersync (the two delegate to different asupersync
    /// acquire entry points: `Semaphore::acquire(cx, n)` vs
    /// `OwnedSemaphorePermit::acquire(Arc<Semaphore>, cx, n)`).
    #[test]
    fn semaphore_acquire_owned_with_cx_observes_pre_cancel() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let sem = std::sync::Arc::new(Semaphore::new(0));
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("tick 427 pre-cancel Semaphore::acquire_owned_with_cx test"),
            );

            let started = std::time::Instant::now();
            let result = timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(2),
                sem.clone().acquire_owned_with_cx(&cx),
            )
            .await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "pre-cancelled cx must short-circuit Semaphore::acquire_owned_with_cx promptly; \
                 took {elapsed:?} (outer 2s timeout likely fired)"
            );
            let inner = result.expect("outer timeout must not fire with cx-cancel observation");
            assert!(
                matches!(inner, Err(AcquireError::Cancelled)),
                "pre-cancelled cx must yield AcquireError::Cancelled (not Closed / \
                 PolledAfterCompletion); got: {inner:?}"
            );
        });
    }

    /// ft-xbnl0.2.4 tick 421: `Semaphore::acquire_with_cx` observes cx-cancel.
    ///
    /// Pins the cx-cancel observation on the Semaphore acquire primitive.
    /// With a zero-permit semaphore (no permits available, no releases
    /// coming) and a pre-cancelled cx, `acquire_with_cx` must return
    /// `Err(AcquireError::Cancelled)` promptly rather than blocking
    /// indefinitely waiting for a permit that will never be released.
    ///
    /// Setup:
    /// 1. Construct `Semaphore::new(0)` (fully contended).
    /// 2. Pre-cancel a cx via `cx.cancel_with(User, ...)`.
    /// 3. Wrap `sem.acquire_with_cx(&cx)` in a 2 s outer safety-net
    ///    timeout (via separate live cx) so a non-observing primitive
    ///    would block until the outer fires.
    /// 4. Assert elapsed < 1 s AND `Err(AcquireError::Cancelled)`
    ///    specifically (not `Closed` — the semaphore is not closed).
    ///
    /// Complements the oneshot + broadcast + yield_now cx-cancel tests
    /// (ticks 418-420) with the semaphore concurrency primitive. Used
    /// across the core for rate limiting and bounded-concurrency work
    /// queues — pinning cx-cancel guards those call sites.
    #[test]
    fn semaphore_acquire_with_cx_observes_pre_cancel() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let sem = Semaphore::new(0);
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("tick 421 pre-cancel Semaphore::acquire_with_cx test"),
            );

            let started = std::time::Instant::now();
            let result = timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(2),
                sem.acquire_with_cx(&cx),
            )
            .await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "pre-cancelled cx must short-circuit Semaphore::acquire_with_cx promptly; \
                 took {elapsed:?} (outer 2s timeout likely fired)"
            );
            let inner = result.expect("outer timeout must not fire with cx-cancel observation");
            assert!(
                matches!(inner, Err(AcquireError::Cancelled)),
                "pre-cancelled cx must yield AcquireError::Cancelled (not Closed / \
                 PolledAfterCompletion); got: {inner:?}"
            );
        });
    }

    /// ft-xbnl0.2.4 tick 420: `broadcast_recv_with_cx` observes cx-cancel.
    ///
    /// Pins the cx-cancel observation on the broadcast receive primitive.
    /// With live senders (no disconnect) and no published messages but a
    /// pre-cancelled cx, `broadcast_recv_with_cx` must return Err promptly
    /// rather than blocking indefinitely waiting for a publish that is not
    /// coming.
    ///
    /// Setup:
    /// 1. Create `(tx, _rx_keepalive)` broadcast channel (capacity 4).
    /// 2. Subscribe a second receiver `rx`.
    /// 3. Keep `_tx` and `_rx_keepalive` alive (no disconnect; receiver
    ///    does NOT see Err(Closed)).
    /// 4. Pre-cancel the cx.
    /// 5. Wrap `broadcast_recv_with_cx(&cx, &mut rx)` in a 2 s outer
    ///    safety-net timeout so a non-observing primitive would block
    ///    until the outer fires, making the failure loud.
    /// 6. Assert elapsed < 1 s AND Err returned.
    ///
    /// Completes the core channel-primitive cancel matrix alongside
    /// tick 419's `oneshot_recv_with_cx` test. Broadcast receivers are
    /// used throughout the event fanout path
    /// (`crates/frankenterm-core/src/events.rs`) — pinning cx-cancel
    /// here guards against regressions in the async fanout plumbing.
    #[test]
    fn broadcast_recv_with_cx_observes_pre_cancel() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_tx, _rx_keepalive) = broadcast::channel::<u64>(4);
            let mut rx = _tx.subscribe();

            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("tick 420 pre-cancel broadcast_recv_with_cx test"),
            );

            let started = std::time::Instant::now();
            let result = timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(2),
                broadcast_recv_with_cx(&cx, &mut rx),
            )
            .await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(1),
                "pre-cancelled cx must short-circuit broadcast_recv_with_cx promptly; \
                 took {elapsed:?} (outer 2s timeout likely fired)"
            );
            let inner = result.expect("outer timeout must not fire with cx-cancel observation");
            assert!(
                matches!(inner, Err(broadcast::RecvError::Cancelled)),
                "pre-cancelled cx must cause broadcast_recv_with_cx to return Err(Cancelled), got: {inner:?}"
            );
        });
    }

    /// ft-xbnl0.2.4 tick 419: `oneshot_recv_with_cx` observes cx-cancel.
    ///
    /// Pins the cx-cancel observation on the oneshot receive primitive.
    /// With a still-alive sender (no disconnect) but a pre-cancelled cx,
    /// `oneshot_recv_with_cx` must return `Err` promptly rather than
    /// blocking indefinitely waiting for a send that will never happen.
    ///
    /// Setup:
    /// 1. Create `(tx, rx)` oneshot channel.
    /// 2. Keep `_tx` alive (no drop; receiver does not see disconnect).
    /// 3. Pre-cancel cx via `cx.cancel_with(User, ...)`.
    /// 4. Wrap `oneshot_recv_with_cx(&cx, rx)` in a 2 s outer timeout.
    /// 5. Assert elapsed < 1 s AND result is Err.
    ///
    /// This complements the yield_now_with_cx cancel-checkpoint tests
    /// (tick 418) and pins the same direct-cancel observability on the
    /// oneshot receive path used throughout the core for single-fire
    /// event signalling.
    #[test]
    fn oneshot_recv_with_cx_observes_pre_cancel() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_tx, rx) = oneshot::channel::<u64>();
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("tick 419 pre-cancel oneshot_recv_with_cx test"),
            );

            let started = std::time::Instant::now();
            let result = timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(2),
                oneshot_recv_with_cx(&cx, rx),
            )
            .await;
            let elapsed = started.elapsed();

            // Outer wrapper is a safety-net: if the primitive is
            // NOT cancel-observing it would block 2 s until the
            // timeout fires. With cx-cancel observation, the inner
            // Err arrives well before the outer timeout.
            assert!(
                elapsed < Duration::from_secs(1),
                "pre-cancelled cx must short-circuit oneshot_recv_with_cx promptly; \
                 took {elapsed:?} (outer 2s timeout likely fired)"
            );
            let inner = result.expect("outer timeout must not fire with cx-cancel observation");
            assert!(
                inner.is_err(),
                "pre-cancelled cx must cause oneshot_recv_with_cx to return Err, got: {inner:?}"
            );
        });
    }

    // ========================================================================
    // time module tests
    // ========================================================================

    // ── Signal module tests ──────────────────────────────────────────────

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for the Cx-first Mutex/RwLock primitives
    // (ft-xbnl0.2.x slice). Pin that lock_with_cx / read_with_cx /
    // write_with_cx actually honor the passed-in Cx under virtual time,
    // rather than falling back to cx::for_request() via the legacy path.
    // -------------------------------------------------------------------------
    #[cfg(unix)]
    mod labruntime_sync_primitives_cx {
        use super::*;

        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(seed)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    f().await;
                })
                .expect("spawn lab task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(
                !matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ),
                "LabRuntime got stuck; termination: {:?}",
                report.termination,
            );
        }

        /// `Mutex::lock_with_cx` acquires the guard and the caller observes
        /// the stored value under a live Cx. Pin the happy-path contract.
        #[test]
        fn mutex_lock_with_cx_returns_guard_with_live_cx() {
            run_lab(0x10C5_10C5_C410_4001, || async move {
                let m = Mutex::new(42u32);
                let cx = crate::cx::for_request();
                let guard = m
                    .lock_with_cx(&cx)
                    .await
                    .expect("live explicit Cx must acquire mutex");
                assert_eq!(*guard, 42);
            });
        }

        /// The owned mutex path is the explicit suspension-spanning escape
        /// hatch: its guard remains live across a yield in a Send task and
        /// releases the same underlying mutex when dropped.
        #[test]
        fn mutex_owned_guard_spans_send_task_yield_and_releases() {
            run_lab(0x10C5_10C5_C410_4009, || async move {
                let mutex = OwnedMutex::new(41u32);
                let cx = crate::cx::for_request();
                let mut owned = mutex
                    .lock_owned_with_cx(&cx)
                    .await
                    .expect("live explicit Cx must acquire owned mutex guard");
                assert!(mutex.has_external_owner());
                *owned = 42;
                task::yield_now().await;
                assert_eq!(*owned, 42);
                drop(owned);
                assert!(!mutex.has_external_owner());

                let reacquired = mutex
                    .lock_owned_with_cx(&cx)
                    .await
                    .expect("dropping owned guard must release mutex");
                assert_eq!(*reacquired, 42);
            });
        }

        /// The canonical mutex surface must report explicit-Cx cancellation.
        #[test]
        fn mutex_lock_with_cx_returns_cancelled() {
            run_lab(0x10C5_10C5_C410_4011, || async move {
                let m = Mutex::new(42u32);
                let cx = crate::cx::for_testing();
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("pre-cancel fallible mutex lock"),
                );

                let error = m
                    .lock_with_cx(&cx)
                    .await
                    .err()
                    .expect("pre-cancelled mutex acquire must be fallible");
                assert_eq!(error, LockAcquireError::Cancelled);
            });
        }

        /// A mutex waiter already queued behind an owner observes cancellation
        /// when explicitly repolled, never acquires, and removes itself so a
        /// later live waiter is not stranded behind stale queue state.
        #[test]
        fn mutex_contended_waiter_cancellation_cleans_queue() {
            run_lab(0x10C5_10C5_C410_4021, || async move {
                use std::future::Future as _;
                use std::task::Poll;

                let mutex = Mutex::new(41u32);
                // The guard is !Send, and the LabRuntime task must be Send:
                // scope the owner in a block (rustc's async auto-trait
                // analysis does not honor an explicit `drop()` here) so it
                // is provably dead before the probe acquisition awaits.
                {
                    let owner_cx = crate::cx::for_request();
                    let owner = mutex
                        .lock_with_cx(&owner_cx)
                        .await
                        .expect("owner must acquire mutex");
                    {
                        let waiter_cx = crate::cx::for_testing();
                        let mut waiter = Box::pin(mutex.lock_with_cx(&waiter_cx));
                        let waker = futures::task::noop_waker();
                        let mut task_cx = std::task::Context::from_waker(&waker);

                        assert!(matches!(waiter.as_mut().poll(&mut task_cx), Poll::Pending));
                        waiter_cx.cancel_with(
                            crate::outcome::CancelKind::User,
                            Some("cancel queued mutex waiter"),
                        );
                        assert!(matches!(
                            waiter.as_mut().poll(&mut task_cx),
                            Poll::Ready(Err(LockAcquireError::Cancelled))
                        ));
                    }
                    assert_eq!(*owner, 41, "cancelled waiter must not acquire or mutate");
                }

                let probe_cx = crate::cx::for_request();
                let probe = mutex
                    .lock_with_cx(&probe_cx)
                    .await
                    .expect("cancelled waiter must leave no stale queue entry");
                assert_eq!(*probe, 41);
            });
        }

        /// `RwLock::read_with_cx` acquires a read guard under a live Cx.
        #[test]
        fn rwlock_read_with_cx_returns_guard_with_live_cx() {
            run_lab(0x10C5_10C5_C410_4002, || async move {
                let r = RwLock::new(7u32);
                let cx = crate::cx::for_request();
                let guard = r
                    .read_with_cx(&cx)
                    .await
                    .expect("live explicit Cx must acquire rwlock read guard");
                assert_eq!(*guard, 7);
            });
        }

        /// The canonical read surface reports explicit-Cx cancellation.
        #[test]
        fn rwlock_read_with_cx_returns_cancelled() {
            run_lab(0x10C5_10C5_C410_4012, || async move {
                let r = RwLock::new(7u32);
                let cx = crate::cx::for_testing();
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("pre-cancel fallible rwlock read"),
                );

                let error = r
                    .read_with_cx(&cx)
                    .await
                    .err()
                    .expect("pre-cancelled rwlock read must be fallible");
                assert_eq!(error, LockAcquireError::Cancelled);
            });
        }

        /// A queued read behind a writer observes cancellation on explicit
        /// repoll and leaves the reader queue usable by a later live reader.
        #[test]
        fn rwlock_contended_reader_cancellation_cleans_queue() {
            run_lab(0x10C5_10C5_C410_4022, || async move {
                use std::future::Future as _;
                use std::task::Poll;

                let rwlock = RwLock::new(7u32);
                let owner_cx = crate::cx::for_request();
                let owner = rwlock
                    .write_with_cx(&owner_cx)
                    .await
                    .expect("owner must acquire write guard");
                {
                    let waiter_cx = crate::cx::for_testing();
                    let mut waiter = Box::pin(rwlock.read_with_cx(&waiter_cx));
                    let waker = futures::task::noop_waker();
                    let mut task_cx = std::task::Context::from_waker(&waker);

                    assert!(matches!(waiter.as_mut().poll(&mut task_cx), Poll::Pending));
                    waiter_cx.cancel_with(
                        crate::outcome::CancelKind::User,
                        Some("cancel queued rwlock reader"),
                    );
                    assert!(matches!(
                        waiter.as_mut().poll(&mut task_cx),
                        Poll::Ready(Err(LockAcquireError::Cancelled))
                    ));
                }
                assert_eq!(*owner, 7, "cancelled reader must not acquire");
                drop(owner);

                let probe_cx = crate::cx::for_request();
                let probe = rwlock
                    .read_with_cx(&probe_cx)
                    .await
                    .expect("cancelled reader must leave no stale queue entry");
                assert_eq!(*probe, 7);
            });
        }

        /// `RwLock::write_with_cx` acquires a write guard under a live Cx
        /// and the mutation is visible to a subsequent read.
        #[test]
        fn rwlock_write_with_cx_mutates_under_live_cx() {
            run_lab(0x10C5_10C5_C410_4003, || async move {
                let r = RwLock::new(1u32);
                let cx = crate::cx::for_request();
                {
                    let mut w = r
                        .write_with_cx(&cx)
                        .await
                        .expect("live explicit Cx must acquire rwlock write guard");
                    *w = 100;
                }
                let read_cx = crate::cx::for_request();
                let guard = r
                    .read_with_cx(&read_cx)
                    .await
                    .expect("live explicit Cx must acquire follow-up read guard");
                assert_eq!(*guard, 100, "write_with_cx mutation must be durable");
            });
        }

        /// The canonical write surface reports explicit-Cx cancellation.
        #[test]
        fn rwlock_write_with_cx_returns_cancelled() {
            run_lab(0x10C5_10C5_C410_4013, || async move {
                let r = RwLock::new(1u32);
                let cx = crate::cx::for_testing();
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("pre-cancel fallible rwlock write"),
                );

                let error = r
                    .write_with_cx(&cx)
                    .await
                    .err()
                    .expect("pre-cancelled rwlock write must be fallible");
                assert_eq!(error, LockAcquireError::Cancelled);
            });
        }

        /// A queued writer behind a reader observes cancellation on explicit
        /// repoll and leaves the writer queue usable by a later live writer.
        #[test]
        fn rwlock_contended_writer_cancellation_cleans_queue() {
            run_lab(0x10C5_10C5_C410_4023, || async move {
                use std::future::Future as _;
                use std::task::Poll;

                let rwlock = RwLock::new(1u32);
                let owner_cx = crate::cx::for_request();
                let owner = rwlock
                    .read_with_cx(&owner_cx)
                    .await
                    .expect("owner must acquire read guard");
                {
                    let waiter_cx = crate::cx::for_testing();
                    let mut waiter = Box::pin(rwlock.write_with_cx(&waiter_cx));
                    let waker = futures::task::noop_waker();
                    let mut task_cx = std::task::Context::from_waker(&waker);

                    assert!(matches!(waiter.as_mut().poll(&mut task_cx), Poll::Pending));
                    waiter_cx.cancel_with(
                        crate::outcome::CancelKind::User,
                        Some("cancel queued rwlock writer"),
                    );
                    assert!(matches!(
                        waiter.as_mut().poll(&mut task_cx),
                        Poll::Ready(Err(LockAcquireError::Cancelled))
                    ));
                }
                assert_eq!(*owner, 1, "cancelled writer must not acquire or mutate");
                drop(owner);

                let probe_cx = crate::cx::for_request();
                let mut probe = rwlock
                    .write_with_cx(&probe_cx)
                    .await
                    .expect("cancelled writer must leave no stale queue entry");
                *probe = 2;
                drop(probe);
                assert_eq!(*rwlock.read().await, 2);
            });
        }

        /// Ambient lock helpers must inherit an installed caller context rather
        /// than minting a new full-capability request context. Their historical
        /// infallible signature still panics when that inherited context cannot
        /// acquire, but cancelled work is never allowed to escape its scope.
        #[test]
        fn ambient_locks_do_not_escape_cancelled_installed_cx() {
            run_lab(0x10C5_10C5_C410_4014, || async move {
                use futures::FutureExt as _;

                let installed = crate::cx::Cx::current().expect("lab task installs a Cx");
                installed.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("cancel installed context before ambient locks"),
                );

                let mutex = Mutex::new(11u32);
                assert!(
                    std::panic::AssertUnwindSafe(mutex.lock())
                        .catch_unwind()
                        .await
                        .is_err(),
                    "ambient mutex must inherit the cancelled task context"
                );

                let rwlock = RwLock::new(12u32);
                assert!(
                    std::panic::AssertUnwindSafe(rwlock.read())
                        .catch_unwind()
                        .await
                        .is_err(),
                    "ambient rwlock read must inherit the cancelled task context"
                );
                assert!(
                    std::panic::AssertUnwindSafe(rwlock.write())
                        .catch_unwind()
                        .await
                        .is_err(),
                    "ambient rwlock write must inherit the cancelled task context"
                );
            });
        }

        /// The ambient mutex entry point retains its infallible guard contract
        /// while delegating through the canonical typed explicit-Cx method.
        #[test]
        fn mutex_lock_still_works_after_cx_first_delegation() {
            run_lab(0x10C5_10C5_C410_4004, || async move {
                let m = Mutex::new(99u32);
                let guard = m.lock().await;
                assert_eq!(*guard, 99);
            });
        }

        /// `broadcast::Sender::send_with_cx` delivers to an active
        /// receiver under a live Cx. Pin the happy-path contract.
        #[test]
        fn broadcast_send_with_cx_delivers_under_live_cx() {
            run_lab(0x10C5_10C5_C410_4005, || async move {
                let (tx, mut rx) = broadcast::channel::<u32>(4);
                let cx = crate::cx::for_request();

                let receivers = tx
                    .send_with_cx(&cx, 99)
                    .expect("send_with_cx should succeed");
                assert_eq!(receivers, 1, "one active receiver");

                let v = rx.recv_with_cx(&cx).await.expect("recv");
                assert_eq!(v, 99);
            });
        }

        /// `broadcast::Sender::send_with_cx` returns SendError(value)
        /// when no receivers are alive — matches send() behavior.
        #[test]
        fn broadcast_send_with_cx_surfaces_send_error_when_no_receivers() {
            run_lab(0x10C5_10C5_C410_4006, || async move {
                let (tx, rx) = broadcast::channel::<u32>(4);
                drop(rx);
                let cx = crate::cx::for_request();
                let err = tx
                    .send_with_cx(&cx, 7)
                    .expect_err("no receivers -> SendError");
                assert_eq!(err.0, 7, "SendError must carry the value");
            });
        }

        /// `oneshot::Sender::send_with_cx` delivers and
        /// `oneshot_recv_with_cx` receives — end-to-end Cx-first oneshot
        /// roundtrip.
        #[test]
        fn oneshot_send_with_cx_roundtrip_under_live_cx() {
            run_lab(0x10C5_10C5_C410_4007, || async move {
                let (tx, rx) = oneshot::channel::<String>();
                let cx = crate::cx::for_request();

                tx.send_with_cx(&cx, "hello".to_string())
                    .expect("oneshot send_with_cx should succeed");

                let recv_cx = crate::cx::for_request();
                let v = oneshot_recv_with_cx(&recv_cx, rx)
                    .await
                    .expect("oneshot_recv_with_cx should succeed");
                assert_eq!(v, "hello");
            });
        }

        /// `oneshot::Sender::send_with_cx` returns Err(value) when the
        /// receiver was dropped before the send — matches send() behavior.
        #[test]
        fn oneshot_send_with_cx_returns_value_when_receiver_dropped() {
            run_lab(0x10C5_10C5_C410_4008, || async move {
                let (tx, rx) = oneshot::channel::<u32>();
                drop(rx);
                let cx = crate::cx::for_request();
                let err = tx
                    .send_with_cx(&cx, 42)
                    .expect_err("receiver dropped -> Err(value)");
                assert_eq!(err, 42, "Err must carry the undelivered value");
            });
        }
    }

    // ========================================================================
    // Variant-dispatch coverage for broadcast error enums and Display impls
    // ========================================================================

    #[test]
    fn broadcast_try_recv_closed_channel() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = broadcast::channel::<i32>(16);
            drop(tx);
            let result = broadcast_try_recv(&mut rx);
            match result {
                Err(BroadcastTryRecvError::Closed) => {} // expected
                other => panic!("expected Closed, got {:?}", other),
            }
        });
    }

    #[test]
    fn broadcast_try_recv_lagged_receiver() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = broadcast::channel(2);
            broadcast_send(&tx, 1).expect("send 1");
            broadcast_send(&tx, 2).expect("send 2");
            broadcast_send(&tx, 3).expect("send 3"); // overflows capacity
            let result = broadcast_try_recv(&mut rx);
            match result {
                Err(BroadcastTryRecvError::Lagged(n)) => {
                    assert!(n >= 1, "should have lagged by at least 1 message");
                }
                other => panic!("expected Lagged, got {:?}", other),
            }
        });
    }

    #[test]
    fn broadcast_recv_error_display_lagged() {
        let err = broadcast::RecvError::Lagged(42);
        assert_eq!(err.to_string(), "receiver lagged by 42 messages");
    }

    #[test]
    fn broadcast_recv_error_display_closed() {
        let err = broadcast::RecvError::Closed;
        assert_eq!(err.to_string(), "broadcast channel closed");
    }

    #[test]
    fn broadcast_recv_error_display_cancelled() {
        let err = broadcast::RecvError::Cancelled;
        assert_eq!(err.to_string(), "broadcast receive cancelled");
    }

    #[test]
    fn broadcast_try_recv_error_display_all_variants() {
        let empty = broadcast::TryRecvError::Empty;
        assert_eq!(empty.to_string(), "broadcast channel empty");

        let closed = broadcast::TryRecvError::Closed;
        assert_eq!(closed.to_string(), "broadcast channel closed");

        let lagged = broadcast::TryRecvError::Lagged(7);
        assert_eq!(lagged.to_string(), "receiver lagged by 7 messages");
    }

    #[test]
    fn broadcast_send_error_display() {
        let err = broadcast::SendError(99);
        assert_eq!(err.to_string(), "sending on a closed broadcast channel");
    }

    #[test]
    fn try_acquire_error_display_variant_text() {
        let no_permits = TryAcquireError::NoPermits;
        assert_eq!(no_permits.to_string(), "no semaphore permits available");

        let closed = TryAcquireError::Closed;
        assert_eq!(closed.to_string(), "semaphore closed");
    }

    #[test]
    fn acquire_error_display_cancelled() {
        let err = AcquireError::Cancelled;
        assert_eq!(err.to_string(), "semaphore acquire cancelled");
    }

    #[test]
    fn acquire_error_display_polled_after_completion() {
        let err = AcquireError::PolledAfterCompletion;
        assert_eq!(
            err.to_string(),
            "semaphore acquire future polled after completion"
        );
    }

    #[test]
    fn lock_acquire_error_display_preserves_failure_class() {
        assert_eq!(LockAcquireError::Poisoned.to_string(), "lock is poisoned");
        assert_eq!(
            LockAcquireError::Cancelled.to_string(),
            "lock acquisition cancelled"
        );
        assert_eq!(
            LockAcquireError::DeadlineExceeded.to_string(),
            "lock capability deadline exceeded"
        );
        assert_eq!(
            LockAcquireError::PollQuotaExhausted.to_string(),
            "lock capability poll quota exhausted"
        );
        assert_eq!(
            LockAcquireError::CostBudgetExhausted.to_string(),
            "lock capability cost budget exhausted"
        );
        assert_eq!(
            LockAcquireError::ContextFailure.to_string(),
            "lock capability context failed"
        );
        assert_eq!(
            LockAcquireError::TimedOut { deadline_nanos: 42 }.to_string(),
            "lock acquisition timed out at 42ns"
        );
        assert_eq!(
            LockAcquireError::PolledAfterCompletion.to_string(),
            "lock acquisition future polled after completion"
        );
    }

    #[test]
    fn lock_cancelled_error_preserves_exact_capability_context_class() {
        use crate::outcome::CancelKind;

        let cases = [
            (CancelKind::User, LockAcquireError::Cancelled),
            (CancelKind::Timeout, LockAcquireError::DeadlineExceeded),
            (CancelKind::Deadline, LockAcquireError::DeadlineExceeded),
            (CancelKind::PollQuota, LockAcquireError::PollQuotaExhausted),
            (
                CancelKind::CostBudget,
                LockAcquireError::CostBudgetExhausted,
            ),
            (CancelKind::FailFast, LockAcquireError::Cancelled),
            (CancelKind::RaceLost, LockAcquireError::Cancelled),
            (CancelKind::ParentCancelled, LockAcquireError::Cancelled),
            (CancelKind::ResourceUnavailable, LockAcquireError::Cancelled),
            (CancelKind::Shutdown, LockAcquireError::Cancelled),
            (CancelKind::LinkedExit, LockAcquireError::Cancelled),
        ];

        for (kind, expected) in cases {
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(kind, Some("SECRET exact lock cancellation class"));
            assert_eq!(
                map_mutex_lock_error(&cx, asupersync::sync::LockError::Cancelled),
                expected
            );
            assert_eq!(
                map_rwlock_error(&cx, asupersync::sync::RwLockError::Cancelled),
                expected
            );
        }

        let live_cx = crate::cx::Cx::for_testing();
        assert_eq!(
            map_mutex_lock_error(&live_cx, asupersync::sync::LockError::Cancelled),
            LockAcquireError::ContextFailure
        );
        assert_eq!(
            map_rwlock_error(&live_cx, asupersync::sync::RwLockError::Cancelled),
            LockAcquireError::ContextFailure
        );
    }

    #[test]
    fn broadcast_try_recv_error_is_std_error() {
        let empty: &dyn std::error::Error = &broadcast::TryRecvError::Empty;
        assert!(!empty.to_string().is_empty());

        let closed: &dyn std::error::Error = &broadcast::TryRecvError::Closed;
        assert!(!closed.to_string().is_empty());

        let lagged: &dyn std::error::Error = &broadcast::TryRecvError::Lagged(5);
        assert!(!lagged.to_string().is_empty());
    }
}
