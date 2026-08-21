#[cfg(windows)]
use crate::os::windows::event::EventHandle;
#[cfg(target_os = "macos")]
use core_foundation::base::CFRelease;
#[cfg(target_os = "macos")]
use core_foundation::runloop::*;
use promise::spawn::{
    MainThreadAdmissionLimits, MainThreadAdmissionReceipt, MainThreadEnqueueReceipt,
    MainThreadQueueSnapshot, MainThreadSchedulerBinding, MainThreadSchedulerIdentity,
    MainThreadServiceClass, Runnable, SpawnFunc,
};
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once};
use std::time::Instant;
#[cfg(all(unix, not(target_os = "macos")))]
use {
    filedescriptor::{FileDescriptor, Pipe},
    std::os::unix::io::AsRawFd,
};

lazy_static::lazy_static! {
    pub(crate) static ref SPAWN_QUEUE: Arc<SpawnQueue> = Arc::new(SpawnQueue::new().expect("failed to create SpawnQueue"));
}

const GUI_SPAWN_TASK_CAPACITY: usize = 4_096;
const GUI_SPAWN_ESTIMATED_BYTE_CAPACITY: usize = 64 * 1024 * 1024;
const GUI_SPAWN_CRITICAL_TASK_RESERVE: usize = 512;
const GUI_SPAWN_CRITICAL_ESTIMATED_BYTE_RESERVE: usize = 8 * 1024 * 1024;
const GUI_SPAWN_HIGH_PRIORITY_BURST: usize = 16;
#[cfg(windows)]
const GUI_WINDOWS_DISPATCH_BUDGET: usize = 64;

const WEIGHTED_SERVICE_ORDER: [MainThreadServiceClass; 12] = [
    MainThreadServiceClass::Input,
    MainThreadServiceClass::Topology,
    MainThreadServiceClass::Interactive,
    MainThreadServiceClass::Input,
    MainThreadServiceClass::Topology,
    MainThreadServiceClass::Interactive,
    MainThreadServiceClass::Render,
    MainThreadServiceClass::Input,
    MainThreadServiceClass::Topology,
    MainThreadServiceClass::Interactive,
    MainThreadServiceClass::Render,
    MainThreadServiceClass::Background,
];

struct LegacySpawnFunc {
    func: SpawnFunc,
    at: Instant,
}

struct AdmittedSpawnFunc {
    func: SpawnFunc,
    at: Instant,
    estimated_bytes: NonZeroUsize,
    service_class: MainThreadServiceClass,
    high_priority: bool,
}

struct ServiceQueues {
    input: VecDeque<AdmittedSpawnFunc>,
    topology: VecDeque<AdmittedSpawnFunc>,
    interactive: VecDeque<AdmittedSpawnFunc>,
    render: VecDeque<AdmittedSpawnFunc>,
    background: VecDeque<AdmittedSpawnFunc>,
}

impl ServiceQueues {
    fn with_capacity(capacity: usize) -> Self {
        // Any one service lane may temporarily own every admitted task.  Each
        // deque is therefore reserved to the shared task bound up front; the
        // admission controller ensures their combined live length never
        // exceeds that bound.
        Self {
            input: VecDeque::with_capacity(capacity),
            topology: VecDeque::with_capacity(capacity),
            interactive: VecDeque::with_capacity(capacity),
            render: VecDeque::with_capacity(capacity),
            background: VecDeque::with_capacity(capacity),
        }
    }

    fn queue_mut(
        &mut self,
        service_class: MainThreadServiceClass,
    ) -> &mut VecDeque<AdmittedSpawnFunc> {
        match service_class {
            MainThreadServiceClass::Input => &mut self.input,
            MainThreadServiceClass::Topology => &mut self.topology,
            MainThreadServiceClass::Interactive => &mut self.interactive,
            MainThreadServiceClass::Render => &mut self.render,
            MainThreadServiceClass::Background => &mut self.background,
        }
    }

    fn push_back(&mut self, item: AdmittedSpawnFunc) {
        self.queue_mut(item.service_class).push_back(item);
    }

    fn pop_weighted(&mut self, cursor: &mut usize) -> Option<AdmittedSpawnFunc> {
        for _ in 0..WEIGHTED_SERVICE_ORDER.len() {
            let service_class = WEIGHTED_SERVICE_ORDER[*cursor];
            *cursor = (*cursor + 1) % WEIGHTED_SERVICE_ORDER.len();
            if let Some(item) = self.queue_mut(service_class).pop_front() {
                return Some(item);
            }
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.input.is_empty()
            && self.topology.is_empty()
            && self.interactive.is_empty()
            && self.render.is_empty()
            && self.background.is_empty()
    }

    fn len(&self) -> usize {
        self.input
            .len()
            .checked_add(self.topology.len())
            .and_then(|len| len.checked_add(self.interactive.len()))
            .and_then(|len| len.checked_add(self.render.len()))
            .and_then(|len| len.checked_add(self.background.len()))
            .expect("bounded GUI spawn queue depth overflow")
    }

    fn oldest_enqueued_at(&self) -> Option<Instant> {
        [
            self.input.front(),
            self.topology.front(),
            self.interactive.front(),
            self.render.front(),
            self.background.front(),
        ]
        .iter()
        .flatten()
        .map(|item| item.at)
        .min()
    }
}

struct SpawnQueueState {
    admitted_high: ServiceQueues,
    admitted_low: ServiceQueues,
    // The legacy callbacks cannot report overload, so rejecting here would
    // silently orphan an already-created task. These compatibility lanes are
    // intentionally excluded from bounded receipts and remain transitional;
    // producer migration must remove them rather than pretending they share
    // the admission-aware bound.
    legacy_high: VecDeque<LegacySpawnFunc>,
    legacy_low: VecDeque<LegacySpawnFunc>,
    admitted_estimated_bytes: usize,
    admitted_high_cursor: usize,
    admitted_low_cursor: usize,
    admitted_high_priority_streak: usize,
    legacy_high_priority_streak: usize,
    prefer_admitted: bool,
}

struct SpawnQueueCore {
    identity: MainThreadSchedulerIdentity,
    task_capacity: usize,
    estimated_byte_capacity: usize,
    state: Mutex<SpawnQueueState>,
}

impl SpawnQueueCore {
    fn new(identity: MainThreadSchedulerIdentity, limits: MainThreadAdmissionLimits) -> Self {
        Self {
            identity,
            task_capacity: limits.task_capacity(),
            estimated_byte_capacity: limits.estimated_byte_capacity(),
            state: Mutex::new(SpawnQueueState {
                admitted_high: ServiceQueues::with_capacity(limits.task_capacity()),
                admitted_low: ServiceQueues::with_capacity(limits.task_capacity()),
                legacy_high: VecDeque::new(),
                legacy_low: VecDeque::new(),
                admitted_estimated_bytes: 0,
                admitted_high_cursor: 0,
                admitted_low_cursor: 0,
                admitted_high_priority_streak: 0,
                legacy_high_priority_streak: 0,
                prefer_admitted: true,
            }),
        }
    }

    #[cfg(test)]
    fn queue_legacy(&self, func: SpawnFunc, high_priority: bool) {
        let item = LegacySpawnFunc {
            func,
            at: Instant::now(),
        };
        let mut state = lock_or_recover(&self.state);
        if high_priority {
            state.legacy_high.push_back(item);
        } else {
            state.legacy_low.push_back(item);
        }
    }

    fn enqueue_admitted(
        &self,
        runnable: Runnable,
        admission: MainThreadAdmissionReceipt,
        high_priority: bool,
    ) -> MainThreadEnqueueReceipt {
        assert_eq!(
            (admission.queue_id, admission.scheduler_generation),
            (self.identity.queue_id, self.identity.scheduler_generation),
            "admitted runnable was sent to a different GUI SpawnQueue generation"
        );
        let enqueued_at = Instant::now();
        let mut state = lock_or_recover(&self.state);
        let depth = state
            .admitted_high
            .len()
            .checked_add(state.admitted_low.len())
            .and_then(|depth| depth.checked_add(1))
            .expect("bounded GUI spawn queue depth overflow");
        assert!(
            depth <= self.task_capacity,
            "bounded GUI spawn queue exceeded task-lifetime capacity"
        );
        let estimated_bytes_after = state
            .admitted_estimated_bytes
            .checked_add(admission.estimated_bytes.get())
            .expect("bounded GUI spawn queue byte count overflow");
        assert!(
            estimated_bytes_after <= self.estimated_byte_capacity,
            "bounded GUI spawn queue exceeded task-lifetime byte capacity"
        );

        let item = AdmittedSpawnFunc {
            func: wrap_main_thread_dispatch_scope(Box::new(move || {
                runnable.run();
            })),
            at: enqueued_at,
            estimated_bytes: admission.estimated_bytes,
            service_class: admission.service_class,
            high_priority,
        };
        if high_priority {
            state.admitted_high.push_back(item);
        } else {
            state.admitted_low.push_back(item);
        }
        state.admitted_estimated_bytes = estimated_bytes_after;

        let snapshot_after_enqueue =
            Self::snapshot_locked(&state, self.task_capacity, self.estimated_byte_capacity);
        MainThreadEnqueueReceipt {
            queue_id: admission.queue_id,
            scheduler_generation: admission.scheduler_generation,
            task_ticket: admission.task_ticket,
            enqueued_at,
            snapshot_after_enqueue,
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> MainThreadQueueSnapshot {
        let state = lock_or_recover(&self.state);
        Self::snapshot_locked(&state, self.task_capacity, self.estimated_byte_capacity)
    }

    fn snapshot_locked(
        state: &SpawnQueueState,
        task_capacity: usize,
        estimated_byte_capacity: usize,
    ) -> MainThreadQueueSnapshot {
        let depth = state
            .admitted_high
            .len()
            .checked_add(state.admitted_low.len())
            .expect("bounded GUI spawn queue depth overflow");
        let oldest_enqueued_at = match (
            state.admitted_high.oldest_enqueued_at(),
            state.admitted_low.oldest_enqueued_at(),
        ) {
            (Some(high), Some(low)) => Some(high.min(low)),
            (Some(high), None) => Some(high),
            (None, Some(low)) => Some(low),
            (None, None) => None,
        };
        MainThreadQueueSnapshot::new(
            depth,
            task_capacity,
            state.admitted_estimated_bytes,
            estimated_byte_capacity,
            oldest_enqueued_at,
            false,
        )
        .expect("bounded GUI spawn queue accounting must remain internally consistent")
    }

    fn pop_admitted_locked(state: &mut SpawnQueueState) -> Option<AdmittedSpawnFunc> {
        let have_high = !state.admitted_high.is_empty();
        let have_low = !state.admitted_low.is_empty();
        let take_high = have_high
            && (!have_low || state.admitted_high_priority_streak < GUI_SPAWN_HIGH_PRIORITY_BURST);
        let item = if take_high {
            let mut cursor = state.admitted_high_cursor;
            let item = state.admitted_high.pop_weighted(&mut cursor);
            state.admitted_high_cursor = cursor;
            state.admitted_high_priority_streak =
                state.admitted_high_priority_streak.saturating_add(1);
            item
        } else {
            let mut cursor = state.admitted_low_cursor;
            let item = state.admitted_low.pop_weighted(&mut cursor);
            state.admitted_low_cursor = cursor;
            state.admitted_high_priority_streak = 0;
            item
        }?;
        state.admitted_estimated_bytes = state
            .admitted_estimated_bytes
            .checked_sub(item.estimated_bytes.get())
            .expect("bounded GUI spawn queue byte count underflow");
        Some(item)
    }

    fn pop_legacy_locked(state: &mut SpawnQueueState) -> Option<LegacySpawnFunc> {
        let have_high = !state.legacy_high.is_empty();
        let have_low = !state.legacy_low.is_empty();
        if have_high
            && (!have_low || state.legacy_high_priority_streak < GUI_SPAWN_HIGH_PRIORITY_BURST)
        {
            state.legacy_high_priority_streak = state.legacy_high_priority_streak.saturating_add(1);
            state.legacy_high.pop_front()
        } else {
            state.legacy_high_priority_streak = 0;
            state.legacy_low.pop_front()
        }
    }

    fn pop_func(&self) -> Option<SpawnFunc> {
        enum Popped {
            Admitted(AdmittedSpawnFunc),
            Legacy(LegacySpawnFunc, bool),
        }

        let popped = {
            let mut state = lock_or_recover(&self.state);
            let have_admitted = !state.admitted_high.is_empty() || !state.admitted_low.is_empty();
            let have_legacy = !state.legacy_high.is_empty() || !state.legacy_low.is_empty();
            let take_admitted = have_admitted && (!have_legacy || state.prefer_admitted);
            let popped = if take_admitted {
                Self::pop_admitted_locked(&mut state).map(Popped::Admitted)
            } else {
                let high = !state.legacy_high.is_empty()
                    && (state.legacy_low.is_empty()
                        || state.legacy_high_priority_streak < GUI_SPAWN_HIGH_PRIORITY_BURST);
                Self::pop_legacy_locked(&mut state).map(|item| Popped::Legacy(item, high))
            };
            if have_admitted && have_legacy {
                state.prefer_admitted = !state.prefer_admitted;
            }
            popped
        }?;

        match popped {
            Popped::Admitted(item) => {
                let delay = item.at.elapsed();
                if item.high_priority {
                    metrics::histogram!("executor.spawn_delay").record(delay);
                } else {
                    metrics::histogram!("executor.spawn_delay.low_pri").record(delay);
                }
                match item.service_class {
                    MainThreadServiceClass::Input => {
                        metrics::histogram!("executor.spawn_delay.input").record(delay);
                    }
                    MainThreadServiceClass::Topology => {
                        metrics::histogram!("executor.spawn_delay.topology").record(delay);
                    }
                    MainThreadServiceClass::Interactive => {
                        metrics::histogram!("executor.spawn_delay.interactive").record(delay);
                    }
                    MainThreadServiceClass::Render => {
                        metrics::histogram!("executor.spawn_delay.render").record(delay);
                    }
                    MainThreadServiceClass::Background => {
                        metrics::histogram!("executor.spawn_delay.background").record(delay);
                    }
                }
                Some(item.func)
            }
            Popped::Legacy(item, true) => {
                metrics::histogram!("executor.spawn_delay").record(item.at.elapsed());
                Some(item.func)
            }
            Popped::Legacy(item, false) => {
                metrics::histogram!("executor.spawn_delay.low_pri").record(item.at.elapsed());
                Some(item.func)
            }
        }
    }

    fn has_any_queued(&self) -> bool {
        let state = lock_or_recover(&self.state);
        !state.admitted_high.is_empty()
            || !state.admitted_low.is_empty()
            || !state.legacy_high.is_empty()
            || !state.legacy_low.is_empty()
    }
}

#[derive(Default)]
struct WakeCoalescer {
    pending: AtomicBool,
}

impl WakeCoalescer {
    fn request_signal(&self) -> bool {
        !self.pending.swap(true, Ordering::AcqRel)
    }

    fn begin_rearm(&self) {
        self.pending.store(false, Ordering::Release);
    }

    #[cfg(test)]
    fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
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

fn wrap_main_thread_dispatch_scope(f: SpawnFunc) -> SpawnFunc {
    Box::new(move || {
        // Mark the duration of this task so promise::spawn::block_on
        // can detect a would-be self-deadlock on the main thread and
        // panic with a clear message instead of beach-balling.
        let _dispatch_scope = promise::spawn::enter_main_thread_dispatch_scope();
        f();
    })
}

#[cfg(any(test, target_os = "macos"))]
thread_local! {
    static IN_PLATFORM_TRIGGER: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Prevent nested native event-loop callbacks from executing a second main
/// thread task while the first is still active.  AppKit can re-enter through
/// modal/nested run loops; keeping this helper platform-neutral makes the
/// unwind and re-entry contract directly testable without launching a GUI.
#[cfg(any(test, target_os = "macos"))]
struct PlatformTriggerGuard;

#[cfg(any(test, target_os = "macos"))]
impl Drop for PlatformTriggerGuard {
    fn drop(&mut self) {
        IN_PLATFORM_TRIGGER.with(|active| active.set(false));
    }
}

#[cfg(any(test, target_os = "macos"))]
fn enter_platform_trigger() -> Option<PlatformTriggerGuard> {
    if IN_PLATFORM_TRIGGER.with(|active| active.replace(true)) {
        None
    } else {
        Some(PlatformTriggerGuard)
    }
}

pub(crate) struct SpawnQueue {
    core: SpawnQueueCore,
    wake: WakeCoalescer,
    registration: Once,

    #[cfg(windows)]
    pub event_handle: EventHandle,

    #[cfg(all(unix, not(target_os = "macos")))]
    write: Mutex<FileDescriptor>,
    #[cfg(all(unix, not(target_os = "macos")))]
    read: Mutex<FileDescriptor>,
}

fn schedule_admitted_with_pri(
    runnable: Runnable,
    admission: MainThreadAdmissionReceipt,
    high_priority: bool,
) -> MainThreadEnqueueReceipt {
    SPAWN_QUEUE.enqueue_admitted(runnable, admission, high_priority)
}

impl SpawnQueue {
    pub fn new() -> anyhow::Result<Self> {
        let identity = promise::spawn::try_allocate_main_thread_scheduler_identity()?;
        let limits = MainThreadAdmissionLimits::new(
            GUI_SPAWN_TASK_CAPACITY,
            GUI_SPAWN_ESTIMATED_BYTE_CAPACITY,
            GUI_SPAWN_CRITICAL_TASK_RESERVE,
            GUI_SPAWN_CRITICAL_ESTIMATED_BYTE_RESERVE,
        )?;
        Self::new_impl(SpawnQueueCore::new(identity, limits))
    }

    pub fn register_promise_schedulers(&self) {
        self.registration.call_once(|| {
            let identity = self.core.identity;
            let limits = MainThreadAdmissionLimits::new(
                self.core.task_capacity,
                self.core.estimated_byte_capacity,
                GUI_SPAWN_CRITICAL_TASK_RESERVE,
                GUI_SPAWN_CRITICAL_ESTIMATED_BYTE_RESERVE,
            )
            .expect("SpawnQueue was constructed from these valid limits");
            promise::spawn::set_bounded_main_thread_scheduler(Arc::new(
                MainThreadSchedulerBinding::new(
                    identity,
                    limits,
                    Box::new(|runnable, admission| {
                        schedule_admitted_with_pri(runnable, admission, true)
                    }),
                    Box::new(|runnable, admission| {
                        schedule_admitted_with_pri(runnable, admission, false)
                    }),
                ),
            ));
        });
    }

    pub fn run(&self) -> bool {
        self.run_impl()
    }

    fn execute_budget(&self, budget: usize) {
        for _ in 0..budget {
            let Some(func) = self.core.pop_func() else {
                break;
            };
            // The queue lock was released by pop_func before arbitrary task
            // code runs, including reentrant scheduling callbacks.
            func();
        }
    }

    fn enqueue_admitted(
        &self,
        runnable: Runnable,
        admission: MainThreadAdmissionReceipt,
        high_priority: bool,
    ) -> MainThreadEnqueueReceipt {
        let receipt = self
            .core
            .enqueue_admitted(runnable, admission, high_priority);
        self.request_wakeup();
        receipt
    }

    fn request_wakeup(&self) {
        if self.wake.request_signal() {
            self.signal_platform_wakeup();
        }
    }

    fn finish_run(&self) -> bool {
        // Clear before observing the queue.  An enqueue racing before the
        // clear is found by has_any_queued and re-signalled below; one racing
        // after the clear observes false and signals itself.
        self.wake.begin_rearm();
        let queued = self.core.has_any_queued();
        if queued {
            self.request_wakeup();
        }
        queued
    }
}

#[cfg(windows)]
impl SpawnQueue {
    fn new_impl(core: SpawnQueueCore) -> anyhow::Result<Self> {
        let event_handle = EventHandle::new_manual_reset()
            .map_err(|err| anyhow::anyhow!("EventHandle creation failed: {err:#}"))?;
        Ok(Self {
            core,
            wake: WakeCoalescer::default(),
            registration: Once::new(),
            event_handle,
        })
    }

    fn signal_platform_wakeup(&self) {
        if let Err(err) = self.event_handle.set_event() {
            self.wake.begin_rearm();
            log::error!("Failed to signal GUI spawn queue event: {err:#}");
        }
    }

    fn run_impl(&self) -> bool {
        if let Err(err) = self.event_handle.reset_event() {
            log::error!("Failed to reset GUI spawn queue event: {err:#}");
        }
        self.execute_budget(GUI_WINDOWS_DISPATCH_BUDGET);
        self.finish_run()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl SpawnQueue {
    fn new_impl(core: SpawnQueueCore) -> anyhow::Result<Self> {
        // On linux we have a slightly sloppy wakeup mechanism;
        // we have a non-blocking pipe that we can use to get
        // woken up after some number of enqueues.  We don't
        // guarantee a 1:1 enqueue to wakeup with this mechanism
        // but in practical terms it does guarantee a wakeup
        // if the main thread is asleep and we enqueue some
        // number of items.
        // We can't afford to use a blocking pipe for the wakeup
        // because the write needs to hold a mutex and that
        // can block reads as well as other writers.
        let mut pipe = Pipe::new()?;
        pipe.write.set_non_blocking(true)?;
        pipe.read.set_non_blocking(true)?;
        Ok(Self {
            core,
            wake: WakeCoalescer::default(),
            registration: Once::new(),
            write: Mutex::new(pipe.write),
            read: Mutex::new(pipe.read),
        })
    }

    fn signal_platform_wakeup(&self) {
        use std::io::Write;

        loop {
            match lock_or_recover(&self.write).write(b"x") {
                Ok(1) => break,
                Ok(written) => {
                    self.wake.begin_rearm();
                    log::error!(
                        "GUI spawn queue pipe accepted {written} bytes for a one-byte wake"
                    );
                    break;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    // A full nonblocking pipe is already a durable readable
                    // wake signal. Treat it as successful coalescing.
                    break;
                }
                Err(err) => {
                    self.wake.begin_rearm();
                    log::error!("Failed to signal GUI spawn queue pipe: {err:#}");
                    break;
                }
            }
        }
    }

    fn run_impl(&self) -> bool {
        // On linux we only ever process one at at time, so that
        // we can return to the main loop and process messages
        // from the X server
        self.execute_budget(1);

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

        self.finish_run()
    }

    pub(crate) fn raw_fd(&self) -> std::os::unix::io::RawFd {
        lock_or_recover(&self.read).as_raw_fd()
    }
}

#[cfg(target_os = "macos")]
impl SpawnQueue {
    fn new_impl(core: SpawnQueueCore) -> anyhow::Result<Self> {
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
            // CFRunLoopAddObserver retains the observer.  Release the create
            // reference so constructing the singleton does not leak it.
            CFRelease(observer.cast());
        }

        Ok(Self {
            core,
            wake: WakeCoalescer::default(),
            registration: Once::new(),
        })
    }

    extern "C" fn trigger(
        _observer: *mut __CFRunLoopObserver,
        _: CFRunLoopActivity,
        _: *mut std::ffi::c_void,
    ) {
        let Some(_trigger_guard) = enter_platform_trigger() else {
            return;
        };
        SPAWN_QUEUE.run();
    }

    fn signal_platform_wakeup(&self) {
        unsafe {
            CFRunLoopWakeUp(CFRunLoopGetMain());
        }
    }

    fn run_impl(&self) -> bool {
        self.execute_budget(1);
        self.finish_run()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use promise::spawn::{
        MainThreadAdmissionController, MainThreadAdmissionError, MainThreadAdmissionSnapshot,
    };
    use std::num::{NonZeroU64, NonZeroUsize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_identity() -> MainThreadSchedulerIdentity {
        MainThreadSchedulerIdentity {
            queue_id: NonZeroU64::new(7).unwrap(),
            scheduler_generation: NonZeroU64::new(11).unwrap(),
        }
    }

    fn test_limits(task_capacity: usize) -> MainThreadAdmissionLimits {
        MainThreadAdmissionLimits::new(task_capacity, task_capacity * 64, 0, 0).unwrap()
    }

    fn test_core(task_capacity: usize) -> SpawnQueueCore {
        SpawnQueueCore::new(test_identity(), test_limits(task_capacity))
    }

    fn synthetic_admission(
        ticket: u64,
        service_class: MainThreadServiceClass,
        estimated_bytes: usize,
        task_capacity: usize,
    ) -> MainThreadAdmissionReceipt {
        MainThreadAdmissionReceipt {
            queue_id: test_identity().queue_id,
            scheduler_generation: test_identity().scheduler_generation,
            task_ticket: NonZeroU64::new(ticket).unwrap(),
            service_class,
            estimated_bytes: NonZeroUsize::new(estimated_bytes).unwrap(),
            admitted_at: Instant::now(),
            snapshot_after_admission: MainThreadAdmissionSnapshot {
                active_tasks: 1,
                active_estimated_bytes: estimated_bytes,
                active_general_tasks: if matches!(
                    service_class,
                    MainThreadServiceClass::Input | MainThreadServiceClass::Topology
                ) {
                    0
                } else {
                    1
                },
                active_general_estimated_bytes: if matches!(
                    service_class,
                    MainThreadServiceClass::Input | MainThreadServiceClass::Topology
                ) {
                    0
                } else {
                    estimated_bytes
                },
                task_capacity,
                estimated_byte_capacity: task_capacity * 64,
                retired: false,
            },
        }
    }

    fn enqueue_ready(
        core: &SpawnQueueCore,
        ticket: u64,
        service_class: MainThreadServiceClass,
        high_priority: bool,
        effect: impl FnOnce() + Send + 'static,
    ) -> MainThreadEnqueueReceipt {
        let (runnable, task) = async_task::spawn(async move { effect() }, |_| {});
        task.detach();
        core.enqueue_admitted(
            runnable,
            synthetic_admission(ticket, service_class, 1, core.task_capacity),
            high_priority,
        )
    }

    fn run_one(core: &SpawnQueueCore) {
        core.pop_func().expect("one queued function")();
    }

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

    #[test]
    fn weighted_service_order_preserves_interaction_and_background_progress() {
        let core = test_core(WEIGHTED_SERVICE_ORDER.len());
        let observed = Arc::new(Mutex::new(Vec::new()));

        for (index, service_class) in WEIGHTED_SERVICE_ORDER.iter().copied().enumerate() {
            let observed = Arc::clone(&observed);
            enqueue_ready(&core, index as u64 + 1, service_class, true, move || {
                lock_or_recover(&observed).push(service_class);
            });
        }

        for _ in 0..WEIGHTED_SERVICE_ORDER.len() {
            run_one(&core);
        }
        assert_eq!(
            lock_or_recover(&observed).as_slice(),
            &WEIGHTED_SERVICE_ORDER
        );
        assert_eq!(core.snapshot().depth, 0);
    }

    #[test]
    fn low_priority_work_progresses_after_a_bounded_high_priority_burst() {
        let core = test_core(GUI_SPAWN_HIGH_PRIORITY_BURST + 2);
        let observed = Arc::new(Mutex::new(Vec::new()));

        for value in 0..=GUI_SPAWN_HIGH_PRIORITY_BURST {
            let observed = Arc::clone(&observed);
            enqueue_ready(
                &core,
                value as u64 + 1,
                MainThreadServiceClass::Input,
                true,
                move || lock_or_recover(&observed).push(value),
            );
        }
        let observed_low = Arc::clone(&observed);
        enqueue_ready(
            &core,
            10_000,
            MainThreadServiceClass::Topology,
            false,
            move || lock_or_recover(&observed_low).push(10_000),
        );

        for _ in 0..GUI_SPAWN_HIGH_PRIORITY_BURST + 2 {
            run_one(&core);
        }
        let observed = lock_or_recover(&observed);
        assert_eq!(observed[GUI_SPAWN_HIGH_PRIORITY_BURST], 10_000);
        assert_eq!(observed.len(), GUI_SPAWN_HIGH_PRIORITY_BURST + 2);
    }

    #[test]
    fn critical_reserve_survives_general_lane_saturation() {
        let limits = MainThreadAdmissionLimits::new(4, 64, 2, 16).unwrap();
        let controller = MainThreadAdmissionController::new(
            test_identity().queue_id,
            test_identity().scheduler_generation,
            limits,
        );
        let _render_one = controller
            .try_admit(MainThreadServiceClass::Render, 8)
            .unwrap();
        let _render_two = controller
            .try_admit(MainThreadServiceClass::Background, 8)
            .unwrap();
        assert!(matches!(
            controller.try_admit(MainThreadServiceClass::Interactive, 1),
            Err(MainThreadAdmissionError::TaskCapacityExhausted { .. })
        ));

        let _input = controller
            .try_admit(MainThreadServiceClass::Input, 8)
            .expect("input must consume the critical reserve");
        let _topology = controller
            .try_admit(MainThreadServiceClass::Topology, 8)
            .expect("topology must consume the critical reserve");
        assert_eq!(controller.snapshot().active_tasks, 4);
    }

    #[test]
    fn admitted_queue_receipts_track_exact_depth_bytes_and_oldest_age() {
        let core = test_core(3);
        let first = enqueue_ready(&core, 1, MainThreadServiceClass::Input, true, || {});
        let second = enqueue_ready(&core, 2, MainThreadServiceClass::Render, false, || {});
        assert_eq!(first.snapshot_after_enqueue.depth, 1);
        assert_eq!(first.snapshot_after_enqueue.estimated_bytes, 1);
        assert_eq!(second.snapshot_after_enqueue.depth, 2);
        assert_eq!(second.snapshot_after_enqueue.estimated_bytes, 2);
        assert_eq!(
            second.snapshot_after_enqueue.oldest_enqueued_at,
            Some(first.enqueued_at)
        );

        run_one(&core);
        let after_one = core.snapshot();
        assert_eq!(after_one.depth, 1);
        assert_eq!(after_one.estimated_bytes, 1);
        run_one(&core);
        assert_eq!(
            core.snapshot(),
            MainThreadQueueSnapshot::new(0, 3, 0, 3 * 64, None, false).unwrap()
        );
    }

    #[test]
    fn cancelled_task_releases_permit_when_its_queued_runnable_is_drained() {
        let limits = test_limits(1);
        let controller = MainThreadAdmissionController::new(
            test_identity().queue_id,
            test_identity().scheduler_generation,
            limits,
        );
        let core = test_core(1);
        let ran = Arc::new(AtomicBool::new(false));
        let permit = controller
            .try_admit(MainThreadServiceClass::Input, 1)
            .unwrap();
        let admission = permit.receipt();
        let ran_in_task = Arc::clone(&ran);
        let (runnable, task) = async_task::spawn(
            permit.bind(async move {
                ran_in_task.store(true, Ordering::Release);
            }),
            |_| {},
        );
        core.enqueue_admitted(runnable, admission, true);
        drop(task);

        run_one(&core);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(core.snapshot().depth, 0);
        assert_eq!(controller.snapshot().active_tasks, 0);
    }

    #[test]
    fn self_wake_requeues_without_a_second_task_admission() {
        let limits = test_limits(1);
        let controller = MainThreadAdmissionController::new(
            test_identity().queue_id,
            test_identity().scheduler_generation,
            limits,
        );
        let core = Arc::new(test_core(1));
        let polls = Arc::new(AtomicUsize::new(0));
        let permit = controller
            .try_admit(MainThreadServiceClass::Input, 1)
            .unwrap();
        let admission = permit.receipt();
        let polls_in_task = Arc::clone(&polls);
        let schedule_core = Arc::clone(&core);
        let (runnable, task) = async_task::spawn(
            permit.bind(std::future::poll_fn(move |cx| {
                if polls_in_task.fetch_add(1, Ordering::AcqRel) == 0 {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(())
                }
            })),
            move |runnable| {
                schedule_core.enqueue_admitted(runnable, admission, true);
            },
        );
        task.detach();
        core.enqueue_admitted(runnable, admission, true);

        run_one(&core);
        assert_eq!(polls.load(Ordering::Acquire), 1);
        assert_eq!(core.snapshot().depth, 1);
        assert_eq!(controller.snapshot().active_tasks, 1);
        run_one(&core);
        assert_eq!(polls.load(Ordering::Acquire), 2);
        assert_eq!(core.snapshot().depth, 0);
        assert_eq!(controller.snapshot().active_tasks, 0);
    }

    #[test]
    fn runnable_executes_without_holding_queue_lock_and_may_schedule_reentrantly() {
        let core = Arc::new(test_core(1));
        let effects = Arc::new(AtomicUsize::new(0));
        let reentrant_core = Arc::clone(&core);
        let reentrant_effects = Arc::clone(&effects);
        core.queue_legacy(
            Box::new(move || {
                reentrant_core.queue_legacy(
                    Box::new(move || {
                        reentrant_effects.fetch_add(1, Ordering::AcqRel);
                    }),
                    true,
                );
            }),
            true,
        );

        run_one(&core);
        assert!(core.has_any_queued());
        run_one(&core);
        assert_eq!(effects.load(Ordering::Acquire), 1);
    }

    #[test]
    fn poisoned_queue_lock_recovers_without_losing_existing_work() {
        let core = test_core(1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = core.state.lock().unwrap();
            panic!("poison queue lock");
        }));
        assert!(result.is_err());

        let effects = Arc::new(AtomicUsize::new(0));
        let effects_in_task = Arc::clone(&effects);
        core.queue_legacy(
            Box::new(move || {
                effects_in_task.fetch_add(1, Ordering::AcqRel);
            }),
            true,
        );
        run_one(&core);
        assert_eq!(effects.load(Ordering::Acquire), 1);
    }

    #[test]
    fn wakeup_requests_coalesce_and_rearm_losslessly() {
        let wake = WakeCoalescer::default();
        assert!(wake.request_signal());
        assert!(wake.is_pending());
        assert!(
            !wake.request_signal(),
            "second enqueue shares the pending wake"
        );

        wake.begin_rearm();
        assert!(!wake.is_pending());
        assert!(
            wake.request_signal(),
            "work remaining after a one-item adapter drain gets a fresh wake"
        );
    }

    #[test]
    fn scheduler_registration_is_idempotent_for_one_queue_generation() {
        let registration = Once::new();
        let calls = AtomicUsize::new(0);
        registration.call_once(|| {
            calls.fetch_add(1, Ordering::AcqRel);
        });
        registration.call_once(|| {
            calls.fetch_add(1, Ordering::AcqRel);
        });
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn platform_trigger_reentry_and_unwind_restore_the_guard() {
        let outer = enter_platform_trigger().expect("first trigger enters");
        assert!(
            enter_platform_trigger().is_none(),
            "nested trigger is rejected"
        );
        drop(outer);
        assert!(enter_platform_trigger().is_some(), "guard clears on drop");

        let result = std::panic::catch_unwind(|| {
            let _guard = enter_platform_trigger().expect("trigger enters before panic");
            panic!("simulate native callback unwind");
        });
        assert!(result.is_err());
        assert!(
            enter_platform_trigger().is_some(),
            "guard clears while unwinding"
        );
    }

    #[test]
    fn concurrent_producers_preserve_exact_bounded_accounting() {
        const PRODUCERS: usize = 128;
        let core = Arc::new(test_core(PRODUCERS));
        let effects = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for index in 0..PRODUCERS {
                let core = Arc::clone(&core);
                let effects = Arc::clone(&effects);
                scope.spawn(move || {
                    enqueue_ready(
                        &core,
                        index as u64 + 1,
                        if index % 2 == 0 {
                            MainThreadServiceClass::Input
                        } else {
                            MainThreadServiceClass::Render
                        },
                        index % 3 != 0,
                        move || {
                            effects.fetch_add(1, Ordering::AcqRel);
                        },
                    );
                });
            }
        });

        let full = core.snapshot();
        assert_eq!(full.depth, PRODUCERS);
        assert_eq!(full.estimated_bytes, PRODUCERS);
        for _ in 0..PRODUCERS {
            run_one(&core);
        }
        assert_eq!(effects.load(Ordering::Acquire), PRODUCERS);
        assert_eq!(core.snapshot().depth, 0);
    }
}
