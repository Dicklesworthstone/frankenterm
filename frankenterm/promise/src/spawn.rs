use anyhow::{anyhow, Result};
use async_executor::Executor;
use flume::{bounded, unbounded, Receiver, TryRecvError};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Poll, Waker};

pub use async_task::{Runnable, Task};
pub type SpawnFunc = Box<dyn FnOnce() + Send>;
pub type ScheduleFunc = Box<dyn Fn(Runnable) + Send + Sync + 'static>;
type SharedScheduleFunc = Arc<dyn Fn(Runnable) + Send + Sync + 'static>;

fn no_scheduler_configured(_: Runnable) {
    panic!("no scheduler has been configured");
}

lazy_static::lazy_static! {
    static ref ON_MAIN_THREAD: Mutex<SharedScheduleFunc> =
        Mutex::new(Arc::new(no_scheduler_configured));
    static ref ON_MAIN_THREAD_LOW_PRI: Mutex<SharedScheduleFunc> =
        Mutex::new(Arc::new(no_scheduler_configured));
    static ref SCOPED_EXECUTOR: Mutex<Option<Arc<Executor<'static>>>> = Mutex::new(None);
}

static SCHEDULER_CONFIGURED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "async-asupersync")]
static ASUPERSYNC_RUNTIME: std::sync::LazyLock<asupersync::runtime::Runtime> =
    std::sync::LazyLock::new(|| {
        asupersync::runtime::RuntimeBuilder::current_thread()
            // Install the wall-clock timer driver. WITHOUT this, the builder
            // leaves `timer_driver: None`, so asupersync's `time::sleep` (which
            // backs `promise::spawn::sleep`) takes its documented fallback of
            // spawning a fresh OS thread *per timer* (asupersync time/sleep.rs).
            // Under the mux client's timer-heavy workload that churned ~37
            // threads/sec (751 spawns / 20s in a profile) and dominated idle
            // CPU. With the driver installed, sleeps register with the runtime's
            // timer wheel and the worker fires expired timers in its park loop —
            // no thread-per-sleep.
            .with_timer_driver(asupersync::time::TimerDriverHandle::with_wall_clock())
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

    // Holds the waker that may later observe
    // during the Future::poll call.
    struct WakerHolder {
        waker: Mutex<Option<Waker>>,
    }

    let holder = Arc::new(WakerHolder {
        waker: Mutex::new(None),
    });

    let thread_waker = Arc::clone(&holder);
    let thread_tx = tx.clone();
    if let Err(err) = std::thread::Builder::new()
        .name("promise-worker".to_string())
        .spawn(move || {
            let res = f();
            if thread_tx.send(res).is_err() {
                return;
            }
            // If someone polled the thread before we got here,
            // they will have populated the waker; extract it
            // and wake up the scheduler so that it will poll
            // the result again. Wake outside the mutex guard so a
            // custom waker cannot poison the holder lock.
            let waker = lock_or_recover(&thread_waker.waker).take();
            if let Some(waker) = waker {
                waker.wake();
            }
        })
    {
        let _ = tx.send(Err(anyhow!("failed to spawn promise worker thread: {err}")));
    }

    struct PendingResult<T> {
        rx: Receiver<Result<T>>,
        holder: Arc<WakerHolder>,
    }

    impl<T> std::future::Future for PendingResult<T> {
        type Output = Result<T>;

        fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
            match self.rx.try_recv() {
                Ok(res) => Poll::Ready(res),
                Err(TryRecvError::Empty) => {
                    let mut waker = lock_or_recover(&self.holder.waker);
                    waker.replace(cx.waker().clone());
                    Poll::Pending
                }
                Err(TryRecvError::Disconnected) => {
                    Poll::Ready(Err(anyhow!("thread terminated without providing a result")))
                }
            }
        }
    }

    spawn_into_main_thread(PendingResult { rx, holder })
}

fn get_scoped() -> Option<Arc<Executor<'static>>> {
    lock_or_recover(&SCOPED_EXECUTOR).as_ref().map(Arc::clone)
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
/// short mux RPCs this is used for, and the behavior `smol::block_on` had before
/// the asupersync migration. A future, fully-async rewrite of those sync write
/// paths is the proper end state.)
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
}

pub struct ScopedExecutor {}

impl Default for ScopedExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopedExecutor {
    pub fn new() -> Self {
        lock_or_recover(&SCOPED_EXECUTOR).replace(Arc::new(Executor::new()));

        Self {}
    }

    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        get_scoped()
            .expect("SCOPED_EXECUTOR to be alive as long as ScopedExecutor")
            .run(future)
            .await
    }
}

impl Drop for ScopedExecutor {
    fn drop(&mut self) {
        lock_or_recover(&SCOPED_EXECUTOR).take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    // Serialize spawn tests that touch global scheduler state
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

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
        // Ensure no scoped executor is active
        SCOPED_EXECUTOR.lock().unwrap().take();
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
