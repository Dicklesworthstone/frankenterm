use anyhow::{anyhow, Result};
use async_executor::Executor;
use flume::{bounded, unbounded, Receiver};
use std::future::Future;
use std::num::{NonZeroU64, NonZeroUsize};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::thread::ThreadId;
use std::time::Instant;
use thiserror::Error;

pub use async_task::{Runnable, Task};
pub type SpawnFunc = Box<dyn FnOnce() + Send>;
pub type ScheduleFunc = Box<dyn Fn(Runnable) + Send + Sync + 'static>;
type SharedScheduleFunc = Arc<dyn Fn(Runnable) + Send + Sync + 'static>;

/// Semantic service lane for work admitted to the process main-thread
/// scheduler.
///
/// Correctness-critical input and topology work may consume the capacity held
/// in reserve for overload recovery. Other work is constrained to the general
/// pool even while that reserve is idle, so a paint or background flood cannot
/// make the application unable to process input, close, or topology work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MainThreadServiceClass {
    Input,
    Topology,
    Interactive,
    Render,
    Background,
}

impl MainThreadServiceClass {
    fn is_correctness_critical(self) -> bool {
        matches!(self, Self::Input | Self::Topology)
    }
}

/// Finite task-lifetime limits for one exact main-thread scheduler generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainThreadAdmissionLimits {
    task_capacity: NonZeroUsize,
    estimated_byte_capacity: NonZeroUsize,
    reserved_critical_tasks: usize,
    reserved_critical_estimated_bytes: usize,
}

impl MainThreadAdmissionLimits {
    pub fn new(
        task_capacity: usize,
        estimated_byte_capacity: usize,
        reserved_critical_tasks: usize,
        reserved_critical_estimated_bytes: usize,
    ) -> std::result::Result<Self, MainThreadAdmissionConfigError> {
        let Some(task_capacity) = NonZeroUsize::new(task_capacity) else {
            return Err(MainThreadAdmissionConfigError::ZeroTaskCapacity);
        };
        let Some(estimated_byte_capacity) = NonZeroUsize::new(estimated_byte_capacity) else {
            return Err(MainThreadAdmissionConfigError::ZeroEstimatedByteCapacity);
        };
        if reserved_critical_tasks > task_capacity.get() {
            return Err(MainThreadAdmissionConfigError::TaskReserveExceedsCapacity {
                reserve: reserved_critical_tasks,
                capacity: task_capacity.get(),
            });
        }
        if reserved_critical_estimated_bytes > estimated_byte_capacity.get() {
            return Err(
                MainThreadAdmissionConfigError::EstimatedByteReserveExceedsCapacity {
                    reserve: reserved_critical_estimated_bytes,
                    capacity: estimated_byte_capacity.get(),
                },
            );
        }
        Ok(Self {
            task_capacity,
            estimated_byte_capacity,
            reserved_critical_tasks,
            reserved_critical_estimated_bytes,
        })
    }

    #[must_use]
    pub fn task_capacity(self) -> usize {
        self.task_capacity.get()
    }

    #[must_use]
    pub fn estimated_byte_capacity(self) -> usize {
        self.estimated_byte_capacity.get()
    }

    #[must_use]
    pub fn reserved_critical_tasks(self) -> usize {
        self.reserved_critical_tasks
    }

    #[must_use]
    pub fn reserved_critical_estimated_bytes(self) -> usize {
        self.reserved_critical_estimated_bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MainThreadAdmissionConfigError {
    #[error("main-thread scheduler task capacity must be nonzero")]
    ZeroTaskCapacity,
    #[error("main-thread scheduler estimated-byte capacity must be nonzero")]
    ZeroEstimatedByteCapacity,
    #[error("critical task reserve {reserve} exceeds task capacity {capacity}")]
    TaskReserveExceedsCapacity { reserve: usize, capacity: usize },
    #[error("critical estimated-byte reserve {reserve} exceeds byte capacity {capacity}")]
    EstimatedByteReserveExceedsCapacity { reserve: usize, capacity: usize },
}

/// Callback-free accounting state observed at one admission linearization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainThreadAdmissionSnapshot {
    pub active_tasks: usize,
    pub active_estimated_bytes: usize,
    pub active_general_tasks: usize,
    pub active_general_estimated_bytes: usize,
    pub task_capacity: usize,
    pub estimated_byte_capacity: usize,
    pub retired: bool,
}

/// Exact task-lifetime admission receipt. Queue implementations add enqueue
/// depth and age at the later runnable-enqueue boundary; those values are not
/// fabricated from the number of live task permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainThreadAdmissionReceipt {
    pub queue_id: NonZeroU64,
    pub scheduler_generation: NonZeroU64,
    pub task_ticket: NonZeroU64,
    pub service_class: MainThreadServiceClass,
    pub estimated_bytes: NonZeroUsize,
    pub admitted_at: Instant,
    pub snapshot_after_admission: MainThreadAdmissionSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MainThreadAdmissionError {
    #[error("estimated task size must be nonzero")]
    ZeroEstimatedBytes,
    #[error("scheduler generation is retired")]
    RetiredGeneration,
    #[error("scheduler task ticket authority is exhausted")]
    TicketExhausted,
    #[error(
        "scheduler task capacity exhausted: active={active}, capacity={capacity}, class={service_class:?}"
    )]
    TaskCapacityExhausted {
        active: usize,
        capacity: usize,
        service_class: MainThreadServiceClass,
    },
    #[error(
        "scheduler estimated-byte capacity exhausted: active={active}, requested={requested}, capacity={capacity}, class={service_class:?}"
    )]
    EstimatedByteCapacityExhausted {
        active: usize,
        requested: usize,
        capacity: usize,
        service_class: MainThreadServiceClass,
    },
}

#[derive(Debug)]
struct MainThreadAdmissionState {
    active_tasks: usize,
    active_estimated_bytes: usize,
    active_general_tasks: usize,
    active_general_estimated_bytes: usize,
    next_ticket: Option<NonZeroU64>,
    retired: bool,
}

#[derive(Debug)]
struct MainThreadAdmissionInner {
    queue_id: NonZeroU64,
    scheduler_generation: NonZeroU64,
    limits: MainThreadAdmissionLimits,
    state: Mutex<MainThreadAdmissionState>,
}

/// Queue-independent task-lifetime capacity authority.
///
/// A permit is acquired before an async task is allocated and held by the
/// future until it completes or is cancelled. Since `async-task` has at most
/// one runnable per task, a queue sized to the same admitted-task capacity can
/// always accept a wake for an already admitted task. Queue implementations
/// must not perform a second fallible slot admission for such wakes.
#[derive(Debug, Clone)]
pub struct MainThreadAdmissionController {
    inner: Arc<MainThreadAdmissionInner>,
}

impl MainThreadAdmissionController {
    #[must_use]
    pub fn new(
        queue_id: NonZeroU64,
        scheduler_generation: NonZeroU64,
        limits: MainThreadAdmissionLimits,
    ) -> Self {
        Self {
            inner: Arc::new(MainThreadAdmissionInner {
                queue_id,
                scheduler_generation,
                limits,
                state: Mutex::new(MainThreadAdmissionState {
                    active_tasks: 0,
                    active_estimated_bytes: 0,
                    active_general_tasks: 0,
                    active_general_estimated_bytes: 0,
                    next_ticket: NonZeroU64::new(1),
                    retired: false,
                }),
            }),
        }
    }

    pub fn try_admit(
        &self,
        service_class: MainThreadServiceClass,
        estimated_bytes: usize,
    ) -> std::result::Result<MainThreadTaskPermit, MainThreadAdmissionError> {
        let Some(estimated_bytes) = NonZeroUsize::new(estimated_bytes) else {
            return Err(MainThreadAdmissionError::ZeroEstimatedBytes);
        };
        let mut state = lock_or_recover(&self.inner.state);
        if state.retired {
            return Err(MainThreadAdmissionError::RetiredGeneration);
        }
        let Some(task_ticket) = state.next_ticket else {
            return Err(MainThreadAdmissionError::TicketExhausted);
        };

        let critical = service_class.is_correctness_critical();
        let task_capacity = if critical {
            self.inner.limits.task_capacity()
        } else {
            self.inner
                .limits
                .task_capacity()
                .saturating_sub(self.inner.limits.reserved_critical_tasks())
        };
        let active_tasks = if critical {
            state.active_tasks
        } else {
            state.active_general_tasks
        };
        if active_tasks >= task_capacity {
            return Err(MainThreadAdmissionError::TaskCapacityExhausted {
                active: active_tasks,
                capacity: task_capacity,
                service_class,
            });
        }

        let estimated_byte_capacity = if critical {
            self.inner.limits.estimated_byte_capacity()
        } else {
            self.inner
                .limits
                .estimated_byte_capacity()
                .saturating_sub(self.inner.limits.reserved_critical_estimated_bytes())
        };
        let active_estimated_bytes = if critical {
            state.active_estimated_bytes
        } else {
            state.active_general_estimated_bytes
        };
        let Some(prospective_estimated_bytes) =
            active_estimated_bytes.checked_add(estimated_bytes.get())
        else {
            return Err(MainThreadAdmissionError::EstimatedByteCapacityExhausted {
                active: active_estimated_bytes,
                requested: estimated_bytes.get(),
                capacity: estimated_byte_capacity,
                service_class,
            });
        };
        if prospective_estimated_bytes > estimated_byte_capacity {
            return Err(MainThreadAdmissionError::EstimatedByteCapacityExhausted {
                active: active_estimated_bytes,
                requested: estimated_bytes.get(),
                capacity: estimated_byte_capacity,
                service_class,
            });
        }

        let Some(total_tasks) = state.active_tasks.checked_add(1) else {
            return Err(MainThreadAdmissionError::TaskCapacityExhausted {
                active: state.active_tasks,
                capacity: self.inner.limits.task_capacity(),
                service_class,
            });
        };
        let Some(total_estimated_bytes) = state
            .active_estimated_bytes
            .checked_add(estimated_bytes.get())
        else {
            return Err(MainThreadAdmissionError::EstimatedByteCapacityExhausted {
                active: state.active_estimated_bytes,
                requested: estimated_bytes.get(),
                capacity: self.inner.limits.estimated_byte_capacity(),
                service_class,
            });
        };
        if total_tasks > self.inner.limits.task_capacity() {
            return Err(MainThreadAdmissionError::TaskCapacityExhausted {
                active: state.active_tasks,
                capacity: self.inner.limits.task_capacity(),
                service_class,
            });
        }
        if total_estimated_bytes > self.inner.limits.estimated_byte_capacity() {
            return Err(MainThreadAdmissionError::EstimatedByteCapacityExhausted {
                active: state.active_estimated_bytes,
                requested: estimated_bytes.get(),
                capacity: self.inner.limits.estimated_byte_capacity(),
                service_class,
            });
        }

        state.active_tasks = total_tasks;
        state.active_estimated_bytes = total_estimated_bytes;
        if !critical {
            state.active_general_tasks = state
                .active_general_tasks
                .checked_add(1)
                .expect("general task count was bounded by total task capacity");
            state.active_general_estimated_bytes = prospective_estimated_bytes;
        }
        state.next_ticket = task_ticket.get().checked_add(1).and_then(NonZeroU64::new);

        let receipt = MainThreadAdmissionReceipt {
            queue_id: self.inner.queue_id,
            scheduler_generation: self.inner.scheduler_generation,
            task_ticket,
            service_class,
            estimated_bytes,
            admitted_at: Instant::now(),
            snapshot_after_admission: self.snapshot_locked(&state),
        };
        Ok(MainThreadTaskPermit {
            inner: MainThreadTaskPermitInner {
                controller: Arc::clone(&self.inner),
                service_class,
                estimated_bytes,
                receipt,
            },
        })
    }

    pub fn retire(&self) {
        lock_or_recover(&self.inner.state).retired = true;
    }

    #[must_use]
    pub fn snapshot(&self) -> MainThreadAdmissionSnapshot {
        let state = lock_or_recover(&self.inner.state);
        self.snapshot_locked(&state)
    }

    fn snapshot_locked(&self, state: &MainThreadAdmissionState) -> MainThreadAdmissionSnapshot {
        MainThreadAdmissionSnapshot {
            active_tasks: state.active_tasks,
            active_estimated_bytes: state.active_estimated_bytes,
            active_general_tasks: state.active_general_tasks,
            active_general_estimated_bytes: state.active_general_estimated_bytes,
            task_capacity: self.inner.limits.task_capacity(),
            estimated_byte_capacity: self.inner.limits.estimated_byte_capacity(),
            retired: state.retired,
        }
    }
}

static MAIN_THREAD_ADMISSION_ACCOUNTING_ERRORS: AtomicU64 = AtomicU64::new(0);

#[must_use]
pub fn main_thread_admission_accounting_errors() -> u64 {
    MAIN_THREAD_ADMISSION_ACCOUNTING_ERRORS.load(Ordering::Relaxed)
}

#[derive(Debug)]
struct MainThreadTaskPermitInner {
    controller: Arc<MainThreadAdmissionInner>,
    service_class: MainThreadServiceClass,
    estimated_bytes: NonZeroUsize,
    receipt: MainThreadAdmissionReceipt,
}

impl Drop for MainThreadTaskPermitInner {
    fn drop(&mut self) {
        let mut state = lock_or_recover(&self.controller.state);
        let Some(active_tasks) = state.active_tasks.checked_sub(1) else {
            state.retired = true;
            MAIN_THREAD_ADMISSION_ACCOUNTING_ERRORS.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(active_estimated_bytes) = state
            .active_estimated_bytes
            .checked_sub(self.estimated_bytes.get())
        else {
            state.retired = true;
            MAIN_THREAD_ADMISSION_ACCOUNTING_ERRORS.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let general_after_release = if self.service_class.is_correctness_critical() {
            None
        } else {
            let Some(active_general_tasks) = state.active_general_tasks.checked_sub(1) else {
                state.retired = true;
                MAIN_THREAD_ADMISSION_ACCOUNTING_ERRORS.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let Some(active_general_estimated_bytes) = state
                .active_general_estimated_bytes
                .checked_sub(self.estimated_bytes.get())
            else {
                state.retired = true;
                MAIN_THREAD_ADMISSION_ACCOUNTING_ERRORS.fetch_add(1, Ordering::Relaxed);
                return;
            };
            Some((active_general_tasks, active_general_estimated_bytes))
        };

        state.active_tasks = active_tasks;
        state.active_estimated_bytes = active_estimated_bytes;
        if let Some((active_general_tasks, active_general_estimated_bytes)) = general_after_release
        {
            state.active_general_tasks = active_general_tasks;
            state.active_general_estimated_bytes = active_general_estimated_bytes;
        }
    }
}

/// Unique task-lifetime capacity permit. This value is intentionally not
/// cloneable: exactly one future owns release authority.
#[derive(Debug)]
#[must_use = "dropping the permit releases its reserved scheduler capacity"]
pub struct MainThreadTaskPermit {
    inner: MainThreadTaskPermitInner,
}

impl MainThreadTaskPermit {
    #[must_use]
    pub fn receipt(&self) -> MainThreadAdmissionReceipt {
        self.inner.receipt
    }

    #[must_use = "the admitted future must be polled or scheduled"]
    pub fn bind<F: Future>(self, future: F) -> MainThreadAdmittedFuture<F> {
        MainThreadAdmittedFuture {
            future: Box::pin(future),
            permit: Some(self),
        }
    }
}

/// Future wrapper that releases task-lifetime scheduler capacity at the exact
/// completion or cancellation boundary.
#[must_use = "futures do nothing unless polled or scheduled"]
pub struct MainThreadAdmittedFuture<F> {
    future: Pin<Box<F>>,
    permit: Option<MainThreadTaskPermit>,
}

impl<F: Future> Future for MainThreadAdmittedFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.future.as_mut().poll(cx) {
            Poll::Ready(output) => {
                drop(this.permit.take());
                Poll::Ready(output)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn no_scheduler_configured(_: Runnable) {
    panic!("no scheduler has been configured");
}

lazy_static::lazy_static! {
    static ref ON_MAIN_THREAD: Mutex<SharedScheduleFunc> =
        Mutex::new(Arc::new(no_scheduler_configured));
    static ref ON_MAIN_THREAD_LOW_PRI: Mutex<SharedScheduleFunc> =
        Mutex::new(Arc::new(no_scheduler_configured));
    static ref SCOPED_EXECUTOR: Mutex<ScopedExecutorRegistry> =
        Mutex::new(ScopedExecutorRegistry::default());
    static ref SCOPED_EXECUTOR_AVAILABLE: Condvar = Condvar::new();
}

static SCHEDULER_CONFIGURED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "async-asupersync")]
static ASUPERSYNC_RUNTIME: std::sync::LazyLock<asupersync::runtime::Runtime> =
    std::sync::LazyLock::new(|| {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build asupersync runtime")
    });

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            mutex.clear_poison();
            poisoned.into_inner()
        }
    }
}

// === Main-thread dispatch detection ===
//
// SpawnQueue dispatchers wrap each popped main-thread task in
// `enter_main_thread_dispatch_scope`. `block_on` queries the flag and
// refuses to run when doing so would self-deadlock the main thread
// (the run loop cannot pump while the dispatcher is blocked here).
thread_local! {
    static IN_MAIN_THREAD_DISPATCH: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// RAII guard returned by [`enter_main_thread_dispatch_scope`]. While
/// held, [`is_in_main_thread_dispatch`] returns `true` on this thread.
///
/// The guard remembers the flag's previous value and restores it on
/// drop, so nested scopes compose correctly: dropping the inner scope
/// does not exit the outer scope's invariant. `!Send` so the guard
/// cannot escape the thread whose flag it owns.
#[must_use = "the dispatch scope ends when this guard is dropped"]
pub struct MainThreadDispatchScope {
    prev: bool,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for MainThreadDispatchScope {
    fn drop(&mut self) {
        IN_MAIN_THREAD_DISPATCH.with(|f| f.set(self.prev));
    }
}

/// Mark this thread as actively running a task popped from the
/// main-thread spawn queue. Returns a guard whose drop restores the
/// previous flag value (so nested scopes are safe even though, in
/// current practice, dispatchers don't nest — re-entry of the CFRunLoop
/// observer is filtered separately in `frankenterm/window/src/spawn.rs`).
pub fn enter_main_thread_dispatch_scope() -> MainThreadDispatchScope {
    let prev = IN_MAIN_THREAD_DISPATCH.with(|f| f.replace(true));
    MainThreadDispatchScope {
        prev,
        _not_send: std::marker::PhantomData,
    }
}

/// True iff this thread is currently inside a task popped from the
/// main-thread spawn queue.
pub fn is_in_main_thread_dispatch() -> bool {
    IN_MAIN_THREAD_DISPATCH.with(|f| f.get())
}

/// Assert that we are NOT inside a main-thread dispatch. Used by
/// `block_on` to refuse self-deadlock.
#[inline]
fn assert_not_in_main_thread_dispatch() {
    if is_in_main_thread_dispatch() {
        block_on_main_thread_panic();
    }
}

#[cold]
#[inline(never)]
fn block_on_main_thread_panic() -> ! {
    panic!(
        "promise::spawn::block_on called while running a task on the \
         main-thread spawn queue: this would deadlock the GUI (the main \
         thread cannot pump its event loop while blocked here). Use an \
         async `.await` path, or hand the blocking work off via \
         `spawn_into_new_thread`."
    );
}

fn schedule_runnable(runnable: Runnable, high_pri: bool) {
    let func = {
        let guard = if high_pri {
            lock_or_recover(&ON_MAIN_THREAD)
        } else {
            lock_or_recover(&ON_MAIN_THREAD_LOW_PRI)
        };
        Arc::clone(&*guard)
    };

    func(runnable);
}

pub fn is_scheduler_configured() -> bool {
    SCHEDULER_CONFIGURED.load(Ordering::Relaxed)
        || lock_or_recover(&SCOPED_EXECUTOR).executor.is_some()
}

/// Set callbacks for scheduling normal and low priority futures.
/// Why this and not "just tokio"?  In a GUI application there is typically
/// a special GUI processing loop that may need to run on the "main thread",
/// so we can't just run a tokio/mio loop in that context.
/// This particular crate has no real knowledge of how that plumbing works,
/// it just provides the abstraction for scheduling the work.
/// This function allows the embedding application to set that up.
pub fn set_schedulers(main: ScheduleFunc, low_pri: ScheduleFunc) {
    *lock_or_recover(&ON_MAIN_THREAD) = Arc::from(main);
    *lock_or_recover(&ON_MAIN_THREAD_LOW_PRI) = Arc::from(low_pri);
    SCHEDULER_CONFIGURED.store(true, Ordering::Relaxed);
}

/// Spawn a new thread to execute the provided function.
/// Returns a JoinHandle that implements the Future trait
/// and that can be used to await and yield the return value
/// from the thread.
/// Can be called from any thread.
pub fn spawn_into_new_thread<F, T>(f: F) -> Task<Result<T>>
where
    F: FnOnce() -> Result<T>,
    F: Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = bounded(1);

    let thread_tx = tx.clone();
    if let Err(err) = std::thread::Builder::new()
        .name("promise-worker".to_string())
        .spawn(move || {
            let res = f();
            let _ = thread_tx.send(res);
        })
    {
        let _ = tx.send(Err(anyhow!("failed to spawn promise worker thread: {err}")));
    }
    drop(tx);

    spawn_into_main_thread(async move {
        match rx.into_recv_async().await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("thread terminated without providing a result")),
        }
    })
}

fn get_scoped() -> Option<Arc<Executor<'static>>> {
    lock_or_recover(&SCOPED_EXECUTOR)
        .executor
        .as_ref()
        .map(Arc::clone)
}

/// Spawn a future into the main thread; it will be polled in the
/// main thread.
/// This function can be called from any thread.
/// If you are on the main thread already, consider using
/// spawn() instead to lift the `Send` requirement.
pub fn spawn_into_main_thread<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    if let Some(executor) = get_scoped() {
        return executor.spawn(future);
    }
    let (runnable, task) = async_task::spawn(future, |runnable| schedule_runnable(runnable, true));
    runnable.schedule();
    task
}

/// Spawn a future into the main thread; it will be polled in
/// the main thread in the low priority queue--all other normal
/// priority items will be drained before considering low priority
/// spawns.
/// If you are on the main thread already, consider using `spawn_with_low_priority`
/// instead to lift the `Send` requirement.
pub fn spawn_into_main_thread_with_low_priority<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    if let Some(executor) = get_scoped() {
        return executor.spawn(future);
    }
    let (runnable, task) = async_task::spawn(future, |runnable| schedule_runnable(runnable, false));
    runnable.schedule();
    task
}

/// Spawn a future with normal priority.
pub fn spawn<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    let (runnable, task) =
        async_task::spawn_local(future, |runnable| schedule_runnable(runnable, true));
    runnable.schedule();
    task
}

/// Spawn a future with low priority; it will be polled only after
/// all other normal priority items are processed.
pub fn spawn_with_low_priority<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    let (runnable, task) =
        async_task::spawn_local(future, |runnable| schedule_runnable(runnable, false));
    runnable.schedule();
    task
}

/// Sleep for the specified duration.
///
/// This helper centralizes timer usage so call-sites can remain runtime-agnostic.
#[cfg(feature = "async-asupersync")]
pub async fn sleep(duration: std::time::Duration) {
    asupersync::time::sleep(asupersync::time::wall_now(), duration).await;
}

/// Sleep for the specified duration.
///
/// This helper centralizes timer usage so call-sites can remain runtime-agnostic.
#[cfg(not(feature = "async-asupersync"))]
pub async fn sleep(duration: std::time::Duration) {
    async_io::Timer::after(duration).await;
}

/// Block the current thread until the passed future completes.
///
/// Panics if called from a task running on the main-thread spawn queue
/// (see [`is_in_main_thread_dispatch`]): blocking the dispatcher
/// deadlocks the GUI because the run loop can't pump while we're parked
/// here.
#[cfg(not(feature = "async-asupersync"))]
pub fn block_on<F: Future>(future: F) -> F::Output {
    assert_not_in_main_thread_dispatch();
    async_io::block_on(future)
}

#[cfg(feature = "async-asupersync")]
pub fn block_on<F: Future>(future: F) -> F::Output {
    assert_not_in_main_thread_dispatch();
    ASUPERSYNC_RUNTIME.block_on(future)
}

/// Run an I/O-bound future to completion with the runtime's reactor actually
/// driving it.
///
/// `block_on` polls its future *directly* on the calling thread and merely
/// parks between wakeups; asupersync only services socket/timer readiness for
/// futures that live on the scheduler, so a future that performs network I/O
/// under plain `block_on` hangs forever the moment it has to wait for bytes
/// that arrive *after* it parks (e.g. a mux handshake reply over an SSH proxy).
/// The local-fast case is masked by readiness fast-paths, which is why it only
/// bites real remote connections.
///
/// `block_on_io` spawns the future as a scheduler-managed task (whose reactor
/// registrations *are* driven) and blocks on its join handle, so I/O wakeups
/// fire correctly. Use this for any future that does socket/timer I/O on a
/// dedicated blocking thread (the mux client reader) OR a short sync-over-async
/// I/O call made from the GUI main thread (e.g. `PaneWriter::write` shipping a
/// keystroke to a remote pane).
///
/// Unlike [`block_on`], this is SAFE to call on the main-thread spawn queue: the
/// spawned task runs on a *separate* runtime worker that drives it (and its I/O)
/// to completion independently, so parking the caller on the join handle cannot
/// self-deadlock the GUI the way a directly-polled `block_on` would. (It does
/// briefly block the event loop until the I/O completes — acceptable for the
/// short mux RPCs this is used for, and the behavior the prior runtime had
/// before the asupersync migration. A future, fully-async rewrite of those sync
/// write paths is the proper end state.)
///
/// The future MUST NOT depend on the calling thread making progress (e.g. it
/// must not block on `spawn_into_main_thread`), or it will deadlock when invoked
/// from the main thread. Mux client RPCs satisfy this: they are serviced by the
/// reader task on the runtime, not by the caller.
#[cfg(feature = "async-asupersync")]
pub fn block_on_io<F>(future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let join = ASUPERSYNC_RUNTIME.handle().spawn(future);
    ASUPERSYNC_RUNTIME.block_on(join)
}

/// Non-asupersync fallback: `async_io::block_on` already drives the global
/// async-io reactor (on its own reactor thread), so it is likewise safe to call
/// from the main thread.
#[cfg(not(feature = "async-asupersync"))]
pub fn block_on_io<F: Future>(future: F) -> F::Output {
    async_io::block_on(future)
}

pub struct SimpleExecutor {
    rx: Receiver<SpawnFunc>,
}

impl Default for SimpleExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl SimpleExecutor {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();

        let tx_main = tx.clone();
        let tx_low = tx.clone();
        let queue_func = move |f: SpawnFunc| {
            tx_main.send(f).ok();
        };
        let queue_func_low = move |f: SpawnFunc| {
            tx_low.send(f).ok();
        };
        set_schedulers(
            Box::new(move |task| {
                queue_func(Box::new(move || {
                    task.run();
                }))
            }),
            Box::new(move |task| {
                queue_func_low(Box::new(move || {
                    task.run();
                }))
            }),
        );
        Self { rx }
    }

    pub fn tick(&self) -> anyhow::Result<()> {
        match self.rx.recv() {
            Ok(func) => func(),
            Err(err) => anyhow::bail!("while waiting for events: {:?}", err),
        };
        Ok(())
    }

    /// Run one queued callback without waiting for work to arrive.
    ///
    /// Test harnesses that install this executor must drain it on the same
    /// thread before replacing the process-global schedulers. A queued local
    /// runnable can contain thread-affine state, so allowing a later test
    /// thread to drop the final scheduler sender would destroy that state on
    /// the wrong thread.
    pub fn try_tick(&self) -> anyhow::Result<bool> {
        match self.rx.try_recv() {
            Ok(func) => {
                func();
                Ok(true)
            }
            Err(flume::TryRecvError::Empty) => Ok(false),
            Err(flume::TryRecvError::Disconnected) => {
                anyhow::bail!("while polling for events: scheduler queue disconnected")
            }
        }
    }
}

#[derive(Default)]
struct ScopedExecutorRegistry {
    executor: Option<Arc<Executor<'static>>>,
    owner: Option<ThreadId>,
    depth: usize,
}

pub struct ScopedExecutor {
    executor: Option<Arc<Executor<'static>>>,
    owner: ThreadId,
}

impl Default for ScopedExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopedExecutor {
    pub fn new() -> Self {
        let owner = std::thread::current().id();
        let mut registry = lock_or_recover(&SCOPED_EXECUTOR);
        loop {
            match (&registry.executor, registry.owner.as_ref()) {
                (Some(executor), Some(current_owner)) if current_owner == &owner => {
                    let executor = Arc::clone(executor);
                    registry.depth = registry
                        .depth
                        .checked_add(1)
                        .expect("scoped executor nesting depth overflow");
                    return Self {
                        executor: Some(executor),
                        owner,
                    };
                }
                (Some(_), Some(_)) => {
                    registry =
                        SCOPED_EXECUTOR_AVAILABLE
                            .wait(registry)
                            .unwrap_or_else(|poisoned| {
                                SCOPED_EXECUTOR.clear_poison();
                                poisoned.into_inner()
                            });
                }
                (None, None) => {
                    let executor = Arc::new(Executor::new());
                    registry.executor = Some(Arc::clone(&executor));
                    registry.owner = Some(owner);
                    registry.depth = 1;
                    return Self {
                        executor: Some(executor),
                        owner,
                    };
                }
                _ => {
                    panic!("scoped executor registry entered an inconsistent state");
                }
            }
        }
    }

    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        self.executor
            .as_ref()
            .expect("ScopedExecutor::run called after executor release")
            .run(future)
            .await
    }
}

impl Drop for ScopedExecutor {
    fn drop(&mut self) {
        let Some(executor) = self.executor.take() else {
            return;
        };
        let registered = {
            let mut registry = lock_or_recover(&SCOPED_EXECUTOR);
            assert_eq!(
                registry.owner.as_ref(),
                Some(&self.owner),
                "scoped executor dropped without owning the registry"
            );
            assert!(
                registry
                    .executor
                    .as_ref()
                    .is_some_and(|registered| Arc::ptr_eq(registered, &executor)),
                "scoped executor dropped after its registry entry was replaced"
            );
            registry.depth = registry
                .depth
                .checked_sub(1)
                .expect("scoped executor nesting depth underflow");
            if registry.depth == 0 {
                registry.owner = None;
                registry.executor.take()
            } else {
                None
            }
        };

        // The executor owns detached futures whose destructors may query this
        // registry. Drop every final reference outside the mutex, and only then
        // admit another thread's scope; otherwise a destructor could dispatch
        // into an unrelated replacement executor.
        let released_registry = registered.is_some();
        drop(registered);
        drop(executor);
        if released_registry {
            SCOPED_EXECUTOR_AVAILABLE.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::{Barrier, Mutex as StdMutex};
    use std::time::{Duration, Instant};

    // Serialize spawn tests that touch global scheduler state
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn admission_controller(
        task_capacity: usize,
        estimated_byte_capacity: usize,
        reserved_critical_tasks: usize,
        reserved_critical_estimated_bytes: usize,
    ) -> MainThreadAdmissionController {
        MainThreadAdmissionController::new(
            NonZeroU64::new(11).unwrap(),
            NonZeroU64::new(17).unwrap(),
            MainThreadAdmissionLimits::new(
                task_capacity,
                estimated_byte_capacity,
                reserved_critical_tasks,
                reserved_critical_estimated_bytes,
            )
            .unwrap(),
        )
    }

    #[test]
    fn admission_limits_reject_zero_and_overcommitted_reserves() {
        assert_eq!(
            MainThreadAdmissionLimits::new(0, 1, 0, 0),
            Err(MainThreadAdmissionConfigError::ZeroTaskCapacity)
        );
        assert_eq!(
            MainThreadAdmissionLimits::new(1, 0, 0, 0),
            Err(MainThreadAdmissionConfigError::ZeroEstimatedByteCapacity)
        );
        assert_eq!(
            MainThreadAdmissionLimits::new(2, 8, 3, 0),
            Err(MainThreadAdmissionConfigError::TaskReserveExceedsCapacity {
                reserve: 3,
                capacity: 2,
            })
        );
        assert_eq!(
            MainThreadAdmissionLimits::new(2, 8, 0, 9),
            Err(
                MainThreadAdmissionConfigError::EstimatedByteReserveExceedsCapacity {
                    reserve: 9,
                    capacity: 8,
                }
            )
        );
    }

    #[test]
    fn admission_reserves_capacity_for_input_and_topology() {
        let controller = admission_controller(3, 30, 1, 10);
        let first = controller
            .try_admit(MainThreadServiceClass::Render, 10)
            .unwrap();
        let second = controller
            .try_admit(MainThreadServiceClass::Interactive, 10)
            .unwrap();
        assert!(matches!(
            controller.try_admit(MainThreadServiceClass::Background, 1),
            Err(MainThreadAdmissionError::TaskCapacityExhausted {
                active: 2,
                capacity: 2,
                service_class: MainThreadServiceClass::Background,
            })
        ));

        let critical = controller
            .try_admit(MainThreadServiceClass::Input, 10)
            .expect("critical input must consume the protected reserve");
        assert_eq!(controller.snapshot().active_tasks, 3);
        assert_eq!(controller.snapshot().active_estimated_bytes, 30);

        drop(second);
        let replacement = controller
            .try_admit(MainThreadServiceClass::Background, 10)
            .expect("releasing one general permit must restore one general slot");
        drop((first, critical, replacement));
        assert_eq!(
            controller.snapshot(),
            MainThreadAdmissionSnapshot {
                active_tasks: 0,
                active_estimated_bytes: 0,
                active_general_tasks: 0,
                active_general_estimated_bytes: 0,
                task_capacity: 3,
                estimated_byte_capacity: 30,
                retired: false,
            }
        );
    }

    #[test]
    fn admission_enforces_estimated_byte_reserve_independently() {
        let controller = admission_controller(8, 100, 0, 40);
        let general = controller
            .try_admit(MainThreadServiceClass::Render, 60)
            .unwrap();
        assert!(matches!(
            controller.try_admit(MainThreadServiceClass::Render, 1),
            Err(MainThreadAdmissionError::EstimatedByteCapacityExhausted {
                active: 60,
                requested: 1,
                capacity: 60,
                service_class: MainThreadServiceClass::Render,
            })
        ));
        let critical = controller
            .try_admit(MainThreadServiceClass::Topology, 40)
            .expect("topology work must retain the protected byte reserve");
        assert_eq!(controller.snapshot().active_estimated_bytes, 100);
        drop((general, critical));
        assert_eq!(controller.snapshot().active_estimated_bytes, 0);
    }

    #[test]
    fn retired_generation_rejects_new_tasks_without_revoking_live_permits() {
        let controller = admission_controller(2, 32, 0, 0);
        let admitted = controller
            .try_admit(MainThreadServiceClass::Interactive, 8)
            .unwrap();
        let receipt = admitted.receipt();
        controller.retire();
        assert!(matches!(
            controller.try_admit(MainThreadServiceClass::Input, 8),
            Err(MainThreadAdmissionError::RetiredGeneration)
        ));
        assert_eq!(receipt.queue_id, NonZeroU64::new(11).unwrap());
        assert_eq!(receipt.scheduler_generation, NonZeroU64::new(17).unwrap());
        assert_eq!(controller.snapshot().active_tasks, 1);
        drop(admitted);
        assert_eq!(controller.snapshot().active_tasks, 0);
        assert!(controller.snapshot().retired);

        let replacement = MainThreadAdmissionController::new(
            NonZeroU64::new(11).unwrap(),
            NonZeroU64::new(18).unwrap(),
            MainThreadAdmissionLimits::new(2, 32, 0, 0).unwrap(),
        );
        assert!(replacement
            .try_admit(MainThreadServiceClass::Input, 8)
            .is_ok());
    }

    #[test]
    fn task_ticket_exhaustion_is_nonwrapping_and_mutation_free() {
        let controller = admission_controller(2, 32, 0, 0);
        lock_or_recover(&controller.inner.state).next_ticket = NonZeroU64::new(u64::MAX);

        let final_ticket = controller
            .try_admit(MainThreadServiceClass::Input, 8)
            .expect("the final nonzero ticket remains usable");
        assert_eq!(final_ticket.receipt().task_ticket.get(), u64::MAX);
        let before = controller.snapshot();
        assert!(matches!(
            controller.try_admit(MainThreadServiceClass::Input, 8),
            Err(MainThreadAdmissionError::TicketExhausted)
        ));
        assert_eq!(controller.snapshot(), before);
        drop(final_ticket);
        assert_eq!(controller.snapshot().active_tasks, 0);
    }

    #[test]
    fn admitted_future_holds_capacity_across_self_wake_and_releases_on_completion() {
        let controller = admission_controller(1, 64, 0, 0);
        let permit = controller
            .try_admit(MainThreadServiceClass::Interactive, 16)
            .unwrap();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let polls_in_task = Arc::clone(&polls);
        let future = std::future::poll_fn(move |cx| {
            if polls_in_task.fetch_add(1, Ordering::SeqCst) == 0 {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(42)
            }
        });
        let (sender, receiver) = unbounded();
        let (runnable, task) = async_task::spawn(permit.bind(future), move |runnable| {
            sender.send(runnable).unwrap();
        });
        runnable.schedule();

        assert_eq!(controller.snapshot().active_tasks, 1);
        receiver.recv().unwrap().run();
        assert_eq!(
            controller.snapshot().active_tasks,
            1,
            "a self-wake must retain its task-lifetime permit between polls"
        );
        receiver.recv().unwrap().run();
        assert_eq!(controller.snapshot().active_tasks, 0);
        assert_eq!(block_on(task), 42);
        assert_eq!(polls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn dropping_scheduled_runnable_releases_cancelled_task_permit() {
        let controller = admission_controller(1, 64, 0, 0);
        let permit = controller
            .try_admit(MainThreadServiceClass::Background, 16)
            .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_in_task = Arc::clone(&dropped);
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let drop_flag = DropFlag(dropped_in_task);
        let future = async move {
            let _drop_flag = drop_flag;
            std::future::pending::<()>().await;
        };
        let (sender, receiver) = unbounded();
        let (runnable, task) = async_task::spawn(permit.bind(future), move |runnable| {
            sender.send(runnable).unwrap();
        });
        runnable.schedule();

        drop(receiver.recv().unwrap());
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(controller.snapshot().active_tasks, 0);
        drop(task);
    }

    #[test]
    fn concurrent_admission_never_oversubscribes_task_capacity() {
        let controller = admission_controller(4, 4_096, 0, 0);
        let barrier = Arc::new(Barrier::new(17));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let controller = controller.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                controller.try_admit(MainThreadServiceClass::Interactive, 1)
            }));
        }
        barrier.wait();

        let admitted = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap().ok())
            .collect::<Vec<_>>();
        assert_eq!(admitted.len(), 4);
        assert_eq!(controller.snapshot().active_tasks, 4);
        drop(admitted);
        assert_eq!(controller.snapshot().active_tasks, 0);
    }

    #[test]
    fn panicking_admitted_future_releases_its_permit() {
        let controller = admission_controller(1, 64, 0, 0);
        let permit = controller
            .try_admit(MainThreadServiceClass::Interactive, 16)
            .unwrap();
        let (sender, receiver) = unbounded();
        let (runnable, task) = async_task::spawn(
            permit.bind(async {
                panic!("intentional admitted-future panic");
            }),
            move |runnable| sender.send(runnable).unwrap(),
        );
        runnable.schedule();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            receiver.recv().unwrap().run();
        }));
        assert!(result.is_err());
        assert_eq!(controller.snapshot().active_tasks, 0);
        drop(task);
    }

    proptest! {
        #[test]
        fn admission_accounting_matches_a_bounded_sequential_model(
            operations in prop::collection::vec(
                (any::<bool>(), 0_u8..5, 1_usize..80, any::<usize>()),
                0..256,
            )
        ) {
            let controller = admission_controller(8, 256, 2, 64);
            let mut held = Vec::new();
            let mut active_tasks = 0_usize;
            let mut active_bytes = 0_usize;
            let mut general_tasks = 0_usize;
            let mut general_bytes = 0_usize;

            for (admit, class_selector, estimated_bytes, removal_selector) in operations {
                if admit {
                    let service_class = match class_selector {
                        0 => MainThreadServiceClass::Input,
                        1 => MainThreadServiceClass::Topology,
                        2 => MainThreadServiceClass::Interactive,
                        3 => MainThreadServiceClass::Render,
                        _ => MainThreadServiceClass::Background,
                    };
                    let critical = service_class.is_correctness_critical();
                    let expected_admission = active_tasks < 8
                        && active_bytes
                            .checked_add(estimated_bytes)
                            .is_some_and(|bytes| bytes <= 256)
                        && (critical
                            || (general_tasks < 6
                                && general_bytes
                                    .checked_add(estimated_bytes)
                                    .is_some_and(|bytes| bytes <= 192)));
                    let result = controller.try_admit(service_class, estimated_bytes);
                    if expected_admission {
                        prop_assert!(result.is_ok());
                        held.push((result.unwrap(), service_class, estimated_bytes));
                        active_tasks += 1;
                        active_bytes += estimated_bytes;
                        if !critical {
                            general_tasks += 1;
                            general_bytes += estimated_bytes;
                        }
                    } else {
                        prop_assert!(result.is_err());
                    }
                } else if !held.is_empty() {
                    let index = removal_selector % held.len();
                    let (permit, service_class, estimated_bytes) = held.swap_remove(index);
                    drop(permit);
                    active_tasks -= 1;
                    active_bytes -= estimated_bytes;
                    if !service_class.is_correctness_critical() {
                        general_tasks -= 1;
                        general_bytes -= estimated_bytes;
                    }
                }

                let snapshot = controller.snapshot();
                prop_assert_eq!(snapshot.active_tasks, active_tasks);
                prop_assert_eq!(snapshot.active_estimated_bytes, active_bytes);
                prop_assert_eq!(snapshot.active_general_tasks, general_tasks);
                prop_assert_eq!(snapshot.active_general_estimated_bytes, general_bytes);
                prop_assert!(!snapshot.retired);
            }

            drop(held);
            prop_assert_eq!(controller.snapshot().active_tasks, 0);
            prop_assert_eq!(controller.snapshot().active_estimated_bytes, 0);
        }
    }

    #[test]
    fn block_on_ready_future() {
        let result = block_on(async { 42 });
        assert_eq!(result, 42);
    }

    #[test]
    fn block_on_with_async_computation() {
        let result = block_on(async {
            let a = 10;
            let b = 20;
            a + b
        });
        assert_eq!(result, 30);
    }

    #[test]
    fn block_on_with_result_type() {
        let result: anyhow::Result<i32> = block_on(async { Ok(99) });
        assert_eq!(result.unwrap(), 99);
    }

    #[test]
    fn block_on_with_string() {
        let result = block_on(async { String::from("hello async") });
        assert_eq!(result, "hello async");
    }

    #[test]
    fn sleep_waits_at_least_requested_duration() {
        let start = Instant::now();
        block_on(sleep(Duration::from_millis(5)));
        assert!(start.elapsed() >= Duration::from_millis(5));
    }

    #[test]
    fn sleep_zero_duration_completes() {
        block_on(sleep(Duration::ZERO));
    }

    #[test]
    fn scoped_executor_creates_and_drops() {
        let _lock = TEST_LOCK.lock().unwrap();
        {
            let _exec = ScopedExecutor::new();
            assert!(get_scoped().is_some());
        }
        // After drop, scoped executor is removed
        assert!(get_scoped().is_none());
    }

    #[test]
    fn scoped_executor_default() {
        let _lock = TEST_LOCK.lock().unwrap();
        {
            let _exec = ScopedExecutor::default();
            assert!(get_scoped().is_some());
        }
        assert!(get_scoped().is_none());
    }

    #[test]
    fn scoped_executor_runs_future() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let result = block_on(exec.run(async { 123 }));
        assert_eq!(result, 123);
        drop(exec);
    }

    #[test]
    fn scoped_executor_spawn_into_main_thread() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_main_thread(async { 456 });
        let result = block_on(exec.run(task));
        assert_eq!(result, 456);
        drop(exec);
    }

    #[test]
    fn scoped_executor_spawn_low_priority() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_main_thread_with_low_priority(async { 789 });
        let result = block_on(exec.run(task));
        assert_eq!(result, 789);
        drop(exec);
    }

    #[test]
    fn scoped_executor_is_a_configured_scheduler() {
        let _lock = TEST_LOCK.lock().unwrap();
        let globally_configured = SCHEDULER_CONFIGURED.swap(false, Ordering::Relaxed);
        let exec = ScopedExecutor::new();

        assert!(is_scheduler_configured());

        drop(exec);
        SCHEDULER_CONFIGURED.store(globally_configured, Ordering::Relaxed);
    }

    #[test]
    fn scoped_executor_releases_registry_before_dropping_futures() {
        struct RegistryLockProbe(Arc<AtomicBool>);

        impl Drop for RegistryLockProbe {
            fn drop(&mut self) {
                self.0
                    .store(SCOPED_EXECUTOR.try_lock().is_ok(), Ordering::Release);
            }
        }

        let _lock = TEST_LOCK.lock().unwrap();
        let observed_unlocked_registry = Arc::new(AtomicBool::new(false));
        let probe = RegistryLockProbe(Arc::clone(&observed_unlocked_registry));
        let exec = ScopedExecutor::new();
        spawn_into_main_thread(async move {
            let _probe = probe;
            std::future::pending::<()>().await;
        })
        .detach();

        drop(exec);

        assert!(
            observed_unlocked_registry.load(Ordering::Acquire),
            "detached future destructors must run after the scoped-executor registry unlocks"
        );
    }

    #[test]
    fn concurrent_scoped_executors_do_not_replace_each_other() {
        let _lock = TEST_LOCK.lock().unwrap();
        let first = ScopedExecutor::new();
        let first_task = spawn_into_main_thread(async { 11 });
        let start = Arc::new(Barrier::new(2));
        let worker_start = Arc::clone(&start);
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_start.wait();
            let second = ScopedExecutor::new();
            acquired_tx
                .send(())
                .expect("test should observe the second scoped executor");
            let second_task = spawn_into_main_thread(async { 22 });
            let result = block_on(second.run(second_task));
            drop(second);
            result
        });

        start.wait();
        assert!(
            acquired_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "a concurrent scope must wait instead of replacing the active executor"
        );
        assert_eq!(block_on(first.run(first_task)), 11);
        drop(first);

        acquired_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("second scope should acquire the registry after release");
        assert_eq!(worker.join().expect("scoped-executor worker"), 22);
    }

    #[test]
    fn simple_executor_configures_scheduler() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _exec = SimpleExecutor::new();
        assert!(is_scheduler_configured());
    }

    #[test]
    fn simple_executor_default() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _exec = SimpleExecutor::default();
        assert!(is_scheduler_configured());
    }

    #[test]
    fn simple_executor_try_tick_is_nonblocking_and_drains_one_callback() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = SimpleExecutor::new();
        assert!(!exec.try_tick().expect("empty queue remains connected"));

        let observed = Arc::new(AtomicBool::new(false));
        let observed_by_task = Arc::clone(&observed);
        let task = spawn_into_main_thread(async move {
            observed_by_task.store(true, Ordering::Release);
        });
        task.detach();

        assert!(exec.try_tick().expect("queued callback must run"));
        assert!(observed.load(Ordering::Acquire));
        assert!(!exec.try_tick().expect("drained queue remains connected"));
    }

    #[test]
    fn set_schedulers_marks_configured() {
        let _lock = TEST_LOCK.lock().unwrap();
        set_schedulers(Box::new(|_| {}), Box::new(|_| {}));
        assert!(is_scheduler_configured());
    }

    #[test]
    fn schedule_callback_runs_outside_scheduler_lock() {
        let _lock = TEST_LOCK.lock().unwrap();
        set_schedulers(
            Box::new(|_| {
                assert!(ON_MAIN_THREAD.try_lock().is_ok());
            }),
            Box::new(|_| {}),
        );

        let (runnable, task) = async_task::spawn(async {}, |_| {});
        schedule_runnable(runnable, true);
        drop(task);
    }

    #[test]
    fn panicking_scheduler_does_not_poison_scheduler_lock() {
        let _lock = TEST_LOCK.lock().unwrap();
        set_schedulers(Box::new(|_| panic!("scheduler panic")), Box::new(|_| {}));

        let (runnable, task) = async_task::spawn(async {}, |_| {});
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            schedule_runnable(runnable, true);
        }));
        assert!(result.is_err());
        drop(task);

        set_schedulers(Box::new(|_| {}), Box::new(|_| {}));
    }

    #[test]
    fn recovered_scheduler_lock_clears_poison() {
        let _lock = TEST_LOCK.lock().unwrap();

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ON_MAIN_THREAD.lock().unwrap();
            panic!("poison scheduler lock for recovery regression");
        }));
        assert!(poisoned.is_err());
        assert!(ON_MAIN_THREAD.is_poisoned());

        set_schedulers(Box::new(|_| {}), Box::new(|_| {}));
        assert!(!ON_MAIN_THREAD.is_poisoned());
    }

    #[test]
    fn spawn_into_new_thread_completes() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_new_thread(|| Ok(42i32));
        let result = block_on(exec.run(task));
        assert_eq!(result.unwrap(), 42);
        drop(exec);
    }

    #[test]
    fn spawn_into_new_thread_with_error() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_new_thread(|| -> anyhow::Result<i32> {
            Err(anyhow::anyhow!("thread error"))
        });
        let result = block_on(exec.run(task));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "thread error");
        drop(exec);
    }

    #[test]
    fn spawn_into_new_thread_with_computation() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_new_thread(|| {
            let sum: i32 = (1..=10).sum();
            Ok(sum)
        });
        let result = block_on(exec.run(task));
        assert_eq!(result.unwrap(), 55);
        drop(exec);
    }

    // ── Additional block_on tests ────────────────────────────

    #[test]
    fn block_on_with_nested_async() {
        let result = block_on(async {
            let inner = async { 10 };
            inner.await + 5
        });
        assert_eq!(result, 15);
    }

    #[test]
    fn block_on_with_unit() {
        block_on(async {});
    }

    #[test]
    fn block_on_with_vec() {
        let result = block_on(async { vec![1, 2, 3] });
        assert_eq!(result, vec![1, 2, 3]);
    }

    // ── Scoped executor additional tests ─────────────────────

    #[test]
    fn scoped_executor_runs_multiple_futures() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let t1 = spawn_into_main_thread(async { 1 });
        let t2 = spawn_into_main_thread(async { 2 });
        let t3 = spawn_into_main_thread(async { 3 });
        let result = block_on(exec.run(async { t1.await + t2.await + t3.await }));
        assert_eq!(result, 6);
        drop(exec);
    }

    #[test]
    fn scoped_executor_chained_async() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let result = block_on(exec.run(async {
            let a = async { 10 }.await;
            let b = async { 20 }.await;
            a + b
        }));
        assert_eq!(result, 30);
        drop(exec);
    }

    #[test]
    fn scoped_executor_spawn_low_priority_with_computation() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_main_thread_with_low_priority(async {
            let sum: i32 = (1..=5).sum();
            sum
        });
        let result = block_on(exec.run(task));
        assert_eq!(result, 15);
        drop(exec);
    }

    // ── spawn_into_new_thread additional tests ───────────────

    #[test]
    fn spawn_into_new_thread_with_sleep() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_new_thread(|| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            Ok(String::from("delayed"))
        });
        let result = block_on(exec.run(task));
        assert_eq!(result.unwrap(), "delayed");
        drop(exec);
    }

    #[test]
    fn spawn_multiple_threads() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let t1 = spawn_into_new_thread(|| Ok(1i32));
        let t2 = spawn_into_new_thread(|| Ok(2i32));
        let t3 = spawn_into_new_thread(|| Ok(3i32));
        let result = block_on(exec.run(async {
            let a = t1.await.unwrap();
            let b = t2.await.unwrap();
            let c = t3.await.unwrap();
            a + b + c
        }));
        assert_eq!(result, 6);
        drop(exec);
    }

    #[test]
    fn spawn_into_new_thread_returns_vec() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_new_thread(|| Ok(vec![1u8, 2, 3, 4, 5]));
        let result = block_on(exec.run(task));
        assert_eq!(result.unwrap(), vec![1, 2, 3, 4, 5]);
        drop(exec);
    }

    // ── get_scoped helper ────────────────────────────────────

    #[test]
    fn get_scoped_none_without_executor() {
        let _lock = TEST_LOCK.lock().unwrap();
        assert!(
            lock_or_recover(&SCOPED_EXECUTOR).executor.is_none(),
            "serialized promise tests must not inherit an active scoped executor"
        );
        assert!(get_scoped().is_none());
    }

    #[test]
    fn get_scoped_some_with_executor() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _exec = ScopedExecutor::new();
        assert!(get_scoped().is_some());
    }

    #[test]
    fn block_on_with_bool() {
        let result = block_on(async { true });
        assert!(result);
    }

    #[test]
    fn block_on_with_option_some() {
        let result = block_on(async { Some(42) });
        assert_eq!(result, Some(42));
    }

    #[test]
    fn block_on_with_option_none() {
        let result: Option<i32> = block_on(async { None });
        assert!(result.is_none());
    }

    #[test]
    fn block_on_with_result_err() {
        let result: anyhow::Result<i32> = block_on(async { Err(anyhow!("async err")) });
        assert_eq!(result.unwrap_err().to_string(), "async err");
    }

    #[test]
    fn scoped_executor_runs_string_future() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let result = block_on(exec.run(async { String::from("scoped") }));
        assert_eq!(result, "scoped");
        drop(exec);
    }

    #[test]
    fn spawn_into_new_thread_with_bool_result() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_new_thread(|| Ok(true));
        let result = block_on(exec.run(task));
        assert!(result.unwrap());
        drop(exec);
    }

    #[test]
    fn scoped_executor_run_with_result_err() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let result: anyhow::Result<i32> = block_on(exec.run(async { Err(anyhow!("scoped err")) }));
        assert_eq!(result.unwrap_err().to_string(), "scoped err");
        drop(exec);
    }

    #[test]
    fn block_on_with_large_computation() {
        let result = block_on(async {
            let sum: u64 = (1..=1000).sum();
            sum
        });
        assert_eq!(result, 500500);
    }

    #[test]
    fn spawn_into_new_thread_with_unit_result() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_new_thread(|| Ok(()));
        let result = block_on(exec.run(task));
        assert!(result.is_ok());
        drop(exec);
    }

    #[test]
    fn scoped_executor_sequential_create_drop() {
        let _lock = TEST_LOCK.lock().unwrap();
        for i in 0..3 {
            let exec = ScopedExecutor::new();
            let result = block_on(exec.run(async move { i }));
            assert_eq!(result, i);
            drop(exec);
            assert!(get_scoped().is_none());
        }
    }

    #[test]
    fn spawn_into_main_thread_with_string() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_main_thread(async { String::from("main thread") });
        let result = block_on(exec.run(task));
        assert_eq!(result, "main thread");
        drop(exec);
    }

    // ── SimpleExecutor construction tests ─────────────────────

    #[test]
    fn simple_executor_new_configures_scheduler() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _exec = SimpleExecutor::new();
        // The constructor should mark scheduler as configured
        assert!(is_scheduler_configured());
    }

    // ── spawn_into_new_thread captured variables ────────────

    #[test]
    fn spawn_into_new_thread_captures_variable() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let captured = String::from("captured value");
        let task = spawn_into_new_thread(move || Ok(captured));
        let result = block_on(exec.run(task));
        assert_eq!(result.unwrap(), "captured value");
        drop(exec);
    }

    #[test]
    fn spawn_into_new_thread_captures_arc() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let shared = Arc::new(StdMutex::new(vec![1, 2, 3]));
        let shared_clone = Arc::clone(&shared);
        let task = spawn_into_new_thread(move || {
            let data = shared_clone.lock().unwrap().clone();
            Ok(data)
        });
        let result = block_on(exec.run(task));
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
        drop(exec);
    }

    // ── block_on deeper nesting ─────────────────────────────

    #[test]
    fn block_on_deeply_nested_async() {
        let result = block_on(async {
            let a = async {
                let b = async {
                    let c = async { 10 };
                    c.await + 5
                };
                b.await * 2
            };
            a.await + 1
        });
        assert_eq!(result, 31); // ((10 + 5) * 2) + 1
    }

    #[test]
    fn block_on_with_tuple() {
        let result = block_on(async { (1, "two", 3.0f64) });
        assert_eq!(result.0, 1);
        assert_eq!(result.1, "two");
        assert!((result.2 - 3.0).abs() < f64::EPSILON);
    }

    // ── Scoped executor with result type ────────────────────

    #[test]
    fn scoped_executor_spawn_returns_result_ok() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_main_thread(async { Ok::<i32, anyhow::Error>(42) });
        let result = block_on(exec.run(task));
        assert_eq!(result.unwrap(), 42);
        drop(exec);
    }

    #[test]
    fn scoped_executor_spawn_returns_result_err() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task =
            spawn_into_main_thread(async { Err::<i32, anyhow::Error>(anyhow!("spawned err")) });
        let result = block_on(exec.run(task));
        assert_eq!(result.unwrap_err().to_string(), "spawned err");
        drop(exec);
    }

    // ── spawn_into_main_thread_with_low_priority additional ──

    #[test]
    fn spawn_low_priority_returns_vec() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_main_thread_with_low_priority(async { vec![10, 20, 30] });
        let result = block_on(exec.run(task));
        assert_eq!(result, vec![10, 20, 30]);
        drop(exec);
    }

    #[test]
    fn spawn_low_priority_multiple_tasks() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let t1 = spawn_into_main_thread_with_low_priority(async { 10 });
        let t2 = spawn_into_main_thread_with_low_priority(async { 20 });
        let result = block_on(exec.run(async { t1.await + t2.await }));
        assert_eq!(result, 30);
        drop(exec);
    }

    // ── Mixed priority tasks ────────────────────────────────

    #[test]
    fn mixed_priority_tasks_all_complete() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let high = spawn_into_main_thread(async { 100 });
        let low = spawn_into_main_thread_with_low_priority(async { 200 });
        let result = block_on(exec.run(async { high.await + low.await }));
        assert_eq!(result, 300);
        drop(exec);
    }

    // ── spawn_into_new_thread with tuple result ─────────────

    #[test]
    fn spawn_into_new_thread_with_tuple() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_new_thread(|| Ok((42, "hello".to_string())));
        let result = block_on(exec.run(task));
        let (num, s) = result.unwrap();
        assert_eq!(num, 42);
        assert_eq!(s, "hello");
        drop(exec);
    }

    // ── block_on with closures producing futures ────────────

    #[test]
    fn block_on_with_async_move() {
        let value = String::from("moved");
        let result = block_on(async move { value.len() });
        assert_eq!(result, 5);
    }

    // ── Sequential scoped executor reuse ────────────────────

    #[test]
    fn scoped_executor_reuse_across_iterations() {
        let _lock = TEST_LOCK.lock().unwrap();
        for i in 0..5 {
            let exec = ScopedExecutor::new();
            let task = spawn_into_main_thread(async move { i * 10 });
            let result = block_on(exec.run(task));
            assert_eq!(result, i * 10);
            drop(exec);
        }
    }

    // ── spawn_into_new_thread heavy computation ─────────────

    #[test]
    fn spawn_into_new_thread_fibonacci() {
        let _lock = TEST_LOCK.lock().unwrap();
        let exec = ScopedExecutor::new();
        let task = spawn_into_new_thread(|| {
            fn fib(n: u64) -> u64 {
                if n <= 1 {
                    return n;
                }
                let mut a = 0u64;
                let mut b = 1u64;
                for _ in 2..=n {
                    let c = a + b;
                    a = b;
                    b = c;
                }
                b
            }
            Ok(fib(20))
        });
        let result = block_on(exec.run(task));
        assert_eq!(result.unwrap(), 6765);
        drop(exec);
    }

    // ── block_on with async chain ───────────────────────────

    #[test]
    fn block_on_async_chain() {
        let result = block_on(async {
            let step1 = async { 1 }.await;
            let step2 = async move { step1 + 2 }.await;
            let step3 = async move { step2 * 3 }.await;
            step3
        });
        assert_eq!(result, 9); // (1 + 2) * 3
    }

    // ── main-thread dispatch scope guard ────────────────────

    // These tests run on their own threads (cargo test default) so the
    // thread-local flag is isolated from any concurrent test.

    #[test]
    fn dispatch_scope_default_false() {
        assert!(!is_in_main_thread_dispatch());
    }

    #[test]
    fn dispatch_scope_sets_and_clears() {
        assert!(!is_in_main_thread_dispatch());
        {
            let _scope = enter_main_thread_dispatch_scope();
            assert!(is_in_main_thread_dispatch());
        }
        assert!(
            !is_in_main_thread_dispatch(),
            "flag must be cleared when the guard drops"
        );
    }

    #[test]
    fn dispatch_scope_save_restore_across_nesting() {
        // Save/restore semantics: inner drop must not exit the outer
        // scope's invariant. This is the bug fix added on 2026-05-24
        // after the cmd+N deadlock review.
        assert!(!is_in_main_thread_dispatch());
        {
            let _outer = enter_main_thread_dispatch_scope();
            assert!(is_in_main_thread_dispatch());
            {
                let _inner = enter_main_thread_dispatch_scope();
                assert!(is_in_main_thread_dispatch());
            }
            assert!(
                is_in_main_thread_dispatch(),
                "outer scope must survive inner guard drop"
            );
        }
        assert!(!is_in_main_thread_dispatch());
    }

    #[test]
    fn block_on_panics_when_called_inside_dispatch_scope() {
        // The thread-local flag is per-test-thread so this is safe to
        // run in parallel with other tests.
        assert!(!is_in_main_thread_dispatch());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = enter_main_thread_dispatch_scope();
            // Should panic before ever evaluating the future.
            let _ = block_on(async { 42 });
        }));
        assert!(
            result.is_err(),
            "block_on must panic when called inside a main-thread dispatch scope"
        );
        // The catch_unwind unwound past the scope's drop, which must
        // have cleared the flag.
        assert!(
            !is_in_main_thread_dispatch(),
            "flag must be cleared on unwind through the guard"
        );
    }

    #[test]
    fn block_on_works_outside_dispatch_scope() {
        // Sanity: outside the scope, block_on still works.
        assert!(!is_in_main_thread_dispatch());
        let result = block_on(async { 7 });
        assert_eq!(result, 7);
    }

    /// Counterpart to `block_on_panics_when_called_inside_dispatch_scope`:
    /// `block_on_io` MUST be usable from the GUI main-thread spawn queue. It
    /// spawns the future onto a runtime worker and joins, so the worker drives
    /// the task (and its I/O) independently of the parked caller — it cannot
    /// trip the main-thread guard and cannot self-deadlock. This is the property
    /// that lets `PaneWriter::write` ship a keystroke to a remote pane and the
    /// mux client reader run, both correctly, after the smol->asupersync move.
    #[test]
    fn block_on_io_is_safe_inside_dispatch_scope() {
        assert!(!is_in_main_thread_dispatch());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = enter_main_thread_dispatch_scope();
            block_on_io(async { 21 + 21 })
        }));
        assert_eq!(
            result.expect("block_on_io must not panic inside a main-thread dispatch scope"),
            42,
        );
        assert!(!is_in_main_thread_dispatch());
    }
}
