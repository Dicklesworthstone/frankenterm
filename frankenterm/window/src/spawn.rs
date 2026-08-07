#[cfg(windows)]
use crate::os::windows::event::EventHandle;
#[cfg(target_os = "macos")]
use core_foundation::runloop::*;
use promise::spawn::{Runnable, SpawnFunc};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
#[cfg(all(unix, not(target_os = "macos")))]
use {
    filedescriptor::{FileDescriptor, Pipe},
    std::os::unix::io::AsRawFd,
};

lazy_static::lazy_static! {
    pub(crate) static ref SPAWN_QUEUE: Arc<SpawnQueue> = Arc::new(SpawnQueue::new().expect("failed to create SpawnQueue"));
}

struct InstrumentedSpawnFunc {
    func: SpawnFunc,
    at: Instant,
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            mutex.clear_poison();
            poisoned.into_inner()
        }
    }
}

fn pop_instrumented_func(
    queue: &Mutex<VecDeque<InstrumentedSpawnFunc>>,
) -> Option<InstrumentedSpawnFunc> {
    lock_or_recover(queue).pop_front()
}

fn wrap_main_thread_dispatch_scope(f: SpawnFunc) -> SpawnFunc {
    Box::new(move || {
        // Mark the duration of this task so promise::spawn::block_on
        // can detect a would-be self-deadlock on the main thread and
        // panic with a clear message instead of beach-balling.
        let _dispatch_scope = promise::spawn::enter_main_thread_dispatch_scope();
        f();
    })
}

pub(crate) struct SpawnQueue {
    spawned_funcs: Mutex<VecDeque<InstrumentedSpawnFunc>>,
    spawned_funcs_low_pri: Mutex<VecDeque<InstrumentedSpawnFunc>>,

    #[cfg(windows)]
    pub event_handle: EventHandle,

    #[cfg(all(unix, not(target_os = "macos")))]
    write: Mutex<FileDescriptor>,
    #[cfg(all(unix, not(target_os = "macos")))]
    read: Mutex<FileDescriptor>,
}

fn schedule_with_pri(runnable: Runnable, high_pri: bool) {
    SPAWN_QUEUE.spawn_impl(
        wrap_main_thread_dispatch_scope(Box::new(move || {
            // `Runnable::run()` returns a bool (whether the task was woken);
            // discard it so the closure is `FnOnce() -> ()` as `SpawnFunc`
            // requires. (The deadlock-guard wrap must not change the unit
            // return contract of the scheduled func.)
            runnable.run();
        })),
        high_pri,
    );
}

impl SpawnQueue {
    pub fn new() -> anyhow::Result<Self> {
        Self::new_impl()
    }

    pub fn register_promise_schedulers(&self) {
        promise::spawn::set_schedulers(
            Box::new(|runnable| {
                schedule_with_pri(runnable, true);
            }),
            Box::new(|runnable| {
                schedule_with_pri(runnable, false);
            }),
        );
    }

    pub fn run(&self) -> bool {
        self.run_impl()
    }

    // This needs to be a separate function from the loop in `run`
    // in order for the lock to be released before we call the
    // returned function
    fn pop_func(&self) -> Option<SpawnFunc> {
        if let Some(func) = pop_instrumented_func(&self.spawned_funcs) {
            metrics::histogram!("executor.spawn_delay").record(func.at.elapsed());
            Some(func.func)
        } else if let Some(func) = pop_instrumented_func(&self.spawned_funcs_low_pri) {
            metrics::histogram!("executor.spawn_delay.low_pri").record(func.at.elapsed());
            Some(func.func)
        } else {
            None
        }
    }

    fn queue_func(&self, f: SpawnFunc, high_pri: bool) {
        let f = InstrumentedSpawnFunc {
            func: f,
            at: Instant::now(),
        };
        if high_pri {
            lock_or_recover(&self.spawned_funcs)
        } else {
            lock_or_recover(&self.spawned_funcs_low_pri)
        }
        .push_back(f);
    }

    fn has_any_queued(&self) -> bool {
        !lock_or_recover(&self.spawned_funcs).is_empty()
            || !lock_or_recover(&self.spawned_funcs_low_pri).is_empty()
    }
}

#[cfg(windows)]
impl SpawnQueue {
    fn new_impl() -> anyhow::Result<Self> {
        let spawned_funcs = Mutex::new(VecDeque::new());
        let spawned_funcs_low_pri = Mutex::new(VecDeque::new());
        let event_handle = EventHandle::new_manual_reset()
            .map_err(|err| anyhow::anyhow!("EventHandle creation failed: {err:#}"))?;
        Ok(Self {
            spawned_funcs,
            spawned_funcs_low_pri,
            event_handle,
        })
    }

    fn spawn_impl(&self, f: SpawnFunc, high_pri: bool) {
        self.queue_func(f, high_pri);
        self.event_handle.set_event();
    }

    fn run_impl(&self) -> bool {
        self.event_handle.reset_event();
        while let Some(func) = self.pop_func() {
            func();
        }
        self.has_any_queued()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl SpawnQueue {
    fn new_impl() -> anyhow::Result<Self> {
        // On linux we have a slightly sloppy wakeup mechanism;
        // we have a non-blocking pipe that we can use to get
        // woken up after some number of enqueues.  We don't
        // guarantee a 1:1 enqueue to wakeup with this mechanism
        // but in practical terms it does guarantee a wakeup
        // if the main thread is asleep and we enqueue some
        // number of items.
        // We can't affort to use a blocking pipe for the wakeup
        // because the write needs to hold a mutex and that
        // can block reads as well as other writers.
        let mut pipe = Pipe::new()?;
        pipe.write.set_non_blocking(true)?;
        pipe.read.set_non_blocking(true)?;
        Ok(Self {
            spawned_funcs: Mutex::new(VecDeque::new()),
            spawned_funcs_low_pri: Mutex::new(VecDeque::new()),
            write: Mutex::new(pipe.write),
            read: Mutex::new(pipe.read),
        })
    }

    fn spawn_impl(&self, f: SpawnFunc, high_pri: bool) {
        use std::io::Write;

        self.queue_func(f, high_pri);
        while let Err(err) = lock_or_recover(&self.write).write(b"x") {
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::warn!("Failed to signal spawn queue pipe: {:#}", err);
            break;
        }
    }

    fn run_impl(&self) -> bool {
        // On linux we only ever process one at at time, so that
        // we can return to the main loop and process messages
        // from the X server
        if let Some(func) = self.pop_func() {
            func();
        }

        // try to drain the pipe.
        // We do this regardless of whether we popped an item
        // so that we avoid being in a perpetually signalled state.
        // It is ok if we completely drain the pipe because the
        // main loop uses the return value to set the sleep
        // interval and will unconditionally call us on each
        // iteration.
        let mut byte = [0u8; 64];
        use std::io::Read;
        let _drained_bytes = lock_or_recover(&self.read).read(&mut byte).unwrap_or(0);

        self.has_any_queued()
    }

    pub(crate) fn raw_fd(&self) -> std::os::unix::io::RawFd {
        lock_or_recover(&self.read).as_raw_fd()
    }
}

#[cfg(target_os = "macos")]
impl SpawnQueue {
    fn new_impl() -> anyhow::Result<Self> {
        let spawned_funcs = Mutex::new(VecDeque::new());
        let spawned_funcs_low_pri = Mutex::new(VecDeque::new());

        let observer = unsafe {
            CFRunLoopObserverCreate(
                std::ptr::null(),
                kCFRunLoopAllActivities,
                1,
                0,
                SpawnQueue::trigger,
                std::ptr::null_mut(),
            )
        };
        anyhow::ensure!(
            !observer.is_null(),
            "CFRunLoopObserverCreate returned null — failed to create run loop observer"
        );
        unsafe {
            CFRunLoopAddObserver(CFRunLoopGetMain(), observer, kCFRunLoopCommonModes);
        }

        Ok(Self {
            spawned_funcs,
            spawned_funcs_low_pri,
        })
    }

    extern "C" fn trigger(
        _observer: *mut __CFRunLoopObserver,
        _: CFRunLoopActivity,
        _: *mut std::ffi::c_void,
    ) {
        // Re-entry guard. The observer is registered for
        // kCFRunLoopAllActivities, so if a task currently being
        // dispatched pumps the run loop synchronously (Cocoa nested
        // event loops, modal panels, anything that calls
        // CFRunLoopRunInMode), this callback can fire again *while we
        // are still inside the previous task*. Popping another task in
        // that state creates nested execution and has historically been
        // a deadlock vector. Skip the re-entry; the work will be picked
        // up the next time the run loop drains cleanly. queue_wakeup is
        // already idempotent so no signal is lost.
        thread_local! {
            static IN_TRIGGER: std::cell::Cell<bool> =
                const { std::cell::Cell::new(false) };
        }
        struct ClearOnDrop;
        impl Drop for ClearOnDrop {
            fn drop(&mut self) {
                IN_TRIGGER.with(|f| f.set(false));
            }
        }

        if IN_TRIGGER.with(|f| f.replace(true)) {
            return;
        }
        let _guard = ClearOnDrop;

        if SPAWN_QUEUE.run() {
            Self::queue_wakeup();
        }
    }

    fn queue_wakeup() {
        unsafe {
            CFRunLoopWakeUp(CFRunLoopGetMain());
        }
    }

    fn spawn_impl(&self, f: SpawnFunc, high_pri: bool) {
        self.queue_func(f, high_pri);
        Self::queue_wakeup();
    }

    fn run_impl(&self) -> bool {
        if let Some(func) = self.pop_func() {
            func();
        }
        self.has_any_queued()
    }
}

#[cfg(test)]
mod tests {
    use super::wrap_main_thread_dispatch_scope;

    #[test]
    fn main_thread_dispatch_wrapper_sets_and_clears_guard() {
        assert!(!promise::spawn::is_in_main_thread_dispatch());

        let wrapped = wrap_main_thread_dispatch_scope(Box::new(|| {
            assert!(promise::spawn::is_in_main_thread_dispatch());
        }));
        wrapped();

        assert!(
            !promise::spawn::is_in_main_thread_dispatch(),
            "dispatch guard must clear after a queued task returns"
        );
    }

    #[test]
    fn main_thread_dispatch_wrapper_makes_block_on_fail_closed() {
        assert!(!promise::spawn::is_in_main_thread_dispatch());

        let wrapped = wrap_main_thread_dispatch_scope(Box::new(|| {
            let _ = promise::spawn::block_on(async { 1 });
        }));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(wrapped));
        assert!(
            result.is_err(),
            "block_on inside a main-thread dispatch task must panic instead of deadlocking"
        );
        assert!(
            !promise::spawn::is_in_main_thread_dispatch(),
            "dispatch guard must clear when a queued task unwinds"
        );
    }
}
