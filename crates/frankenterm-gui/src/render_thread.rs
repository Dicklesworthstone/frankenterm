//! The dedicated render thread (ft-yccm0.4.1.2).
//!
//! With `front_end = "Metal"`, each window builds, encodes and presents its
//! frames on a render thread of its own, so the AppKit main thread only
//! forwards input, window events and IME. A slow frame then never delays
//! input, and a main-thread hiccup never delays a frame. Ghostty runs a
//! renderer thread per surface the same way.
//!
//! The pieces:
//!
//! - A [`FrameDriver`] builds and presents frames. It is created on the
//!   render thread and lives and dies there, so it needs not be `Send`.
//! - The main thread posts the window's surface (pixel size, DPI,
//!   occlusion, focus) to a lock-free [`SurfaceMailbox`]. The render thread
//!   reads it at the start of each frame. When the geometry changed, it calls
//!   [`FrameDriver::reconfigure`] at that frame boundary, never while a frame
//!   is being built. Each frame is therefore built for the drawable size it
//!   is presented at, and live resize shows no stretched frame.
//! - A [`RenderWaker`] asks for a frame. It is `Send + Sync` and coalesces,
//!   so a pane's output notification can call it on whatever thread
//!   delivers it, with no main-thread task per output batch (the ft-4p4uo
//!   saturation class).
//! - The thread's QoS follows the surface: user-interactive while focused,
//!   user-initiated while not, background while occluded. While occluded no
//!   frame is built.
//! - Frames are paced: at most one per [`RenderThread::set_min_frame_interval`]
//!   (the `max_fps` interval). Wakes in between coalesce into the next frame.
//! - Dropping the [`RenderThread`] stops the thread and joins it. The driver
//!   is dropped on the render thread first.

use procinfo::ThreadQos;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

/// QoS of the render thread while its window is focused.
pub const FOCUSED_QOS: ThreadQos = ThreadQos::UserInteractive;
/// QoS of the render thread while its window is visible but not focused:
/// still drawing output the user may be watching.
pub const UNFOCUSED_QOS: ThreadQos = ThreadQos::UserInitiated;
/// QoS of the render thread while its window is occluded and it draws
/// nothing.
pub const OCCLUDED_QOS: ThreadQos = ThreadQos::Background;

/// What the main thread tells the render thread about its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SurfaceState {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub dpi: u32,
    pub occluded: bool,
    pub focused: bool,
}

impl SurfaceState {
    /// What a drawable is sized from: a change needs a reconfigure.
    pub fn geometry(&self) -> (u32, u32, u32) {
        (self.pixel_width, self.pixel_height, self.dpi)
    }

    /// The thread QoS this surface calls for.
    pub fn qos(&self) -> ThreadQos {
        if self.occluded {
            OCCLUDED_QOS
        } else if self.focused {
            FOCUSED_QOS
        } else {
            UNFOCUSED_QOS
        }
    }
}

/// One coherent read of a [`SurfaceMailbox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    pub state: SurfaceState,
    /// How many posts this read reflects; it only grows.
    pub generation: u64,
}

const OCCLUDED: u32 = 1;
const FOCUSED: u32 = 2;

/// A last-writer-wins slot for the window's [`SurfaceState`], read and
/// written without a lock: a sequence lock over atomics.
///
/// A post marks the sequence odd, stores the fields and marks it even again.
/// A read retries until it sees the same even sequence before and after
/// loading the fields, so it never returns a torn mix of two posts. Posts
/// are a few stores and never wait for the reader. Concurrent posters take
/// turns by claiming the odd sequence, though the main thread is the only
/// one in practice.
#[derive(Debug, Default)]
pub struct SurfaceMailbox {
    sequence: AtomicU64,
    size: AtomicU64,
    dpi: AtomicU32,
    flags: AtomicU32,
}

impl SurfaceMailbox {
    pub fn new(state: SurfaceState) -> Self {
        let mailbox = Self::default();
        mailbox.post(state);
        mailbox
    }

    pub fn post(&self, state: SurfaceState) {
        let mut sequence = self.sequence.load(Ordering::Relaxed);
        loop {
            if sequence & 1 == 1 {
                std::hint::spin_loop();
                sequence = self.sequence.load(Ordering::Relaxed);
                continue;
            }
            match self.sequence.compare_exchange_weak(
                sequence,
                sequence + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => sequence = current,
            }
        }
        // The odd sequence is visible before any field changes.
        fence(Ordering::Release);
        self.size.store(
            (u64::from(state.pixel_width) << 32) | u64::from(state.pixel_height),
            Ordering::Relaxed,
        );
        self.dpi.store(state.dpi, Ordering::Relaxed);
        let flags =
            if state.occluded { OCCLUDED } else { 0 } | if state.focused { FOCUSED } else { 0 };
        self.flags.store(flags, Ordering::Relaxed);
        self.sequence.store(sequence + 2, Ordering::Release);
    }

    /// How many posts have completed.
    pub fn generation(&self) -> u64 {
        self.sequence.load(Ordering::Acquire) / 2
    }

    pub fn read(&self) -> Surface {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let size = self.size.load(Ordering::Relaxed);
            let dpi = self.dpi.load(Ordering::Relaxed);
            let flags = self.flags.load(Ordering::Relaxed);
            // The field loads complete before the sequence is checked again.
            fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Relaxed) != before {
                continue;
            }
            return Surface {
                state: SurfaceState {
                    pixel_width: (size >> 32) as u32,
                    pixel_height: size as u32,
                    dpi,
                    occluded: flags & OCCLUDED != 0,
                    focused: flags & FOCUSED != 0,
                },
                generation: before / 2,
            };
        }
    }
}

/// What one [`FrameDriver::frame`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameReport {
    /// A frame was presented.
    pub presented: bool,
    /// Build another frame at this time even without a wake: an animation,
    /// a blinking cursor, or a retry after a drawable timeout.
    pub redraw_at: Option<Instant>,
}

/// Builds and presents a window's frames on its render thread.
pub trait FrameDriver {
    /// The surface geometry changed: size the drawable for `surface` before
    /// the next frame. Runs at a frame boundary, never during
    /// [`Self::frame`].
    fn reconfigure(&mut self, surface: &Surface);

    /// Builds, encodes and presents one frame for `surface`, whose geometry
    /// is the one last passed to [`Self::reconfigure`].
    fn frame(&mut self, surface: &Surface) -> FrameReport;

    /// A display refresh tick passed without a frame. A driver that holds
    /// something from the tick (a display link's drawable) releases it.
    fn tick_skipped(&mut self) {}
}

/// Display refresh ticks for a render thread: a display link
/// (ft-yccm0.4.1.3). It is created on the render thread with the driver and
/// used only there; its [`Self::interrupter`] is the one thing other threads
/// call.
pub trait VsyncSource {
    /// Waits for the next tick while running, until interrupted, or until
    /// `timeout` passes (`None` waits for a tick or an interrupt).
    fn wait(&mut self, timeout: Option<Duration>) -> VsyncWait;
    /// Starts or pauses the ticks. A paused source draws no power and
    /// returns from `wait` only when interrupted or timed out.
    fn set_running(&mut self, running: bool);
    /// Asks the display for a refresh rate.
    fn set_rate_range(&mut self, range: FrameRateRange);
    /// Interrupts a `wait` in progress, or the next one, from any thread.
    fn interrupter(&self) -> Arc<dyn Fn() + Send + Sync>;
}

/// Why [`VsyncSource::wait`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VsyncWait {
    Tick(VsyncTick),
    Interrupted,
    TimedOut,
}

/// One display refresh tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsyncTick {
    /// The display's current refresh period.
    pub interval: Duration,
}

/// A refresh rate range to ask of the display, in frames per second
/// (`CAFrameRateRange`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameRateRange {
    pub minimum: f32,
    pub maximum: f32,
    pub preferred: f32,
}

/// The highest rate asked of a display while busy: ProMotion's 120 Hz.
pub const BUSY_FRAME_RATE: f32 = 120.0;
/// The rate asked while calm: slow output, a blinking cursor.
pub const CALM_FRAME_RATE: f32 = 60.0;
/// Two frames this close together make the window busy.
pub const BUSY_WINDOW: Duration = Duration::from_millis(250);

/// The rate a render thread asks of its display: 120 Hz (ProMotion) while
/// busy with output or interaction, 60 Hz otherwise, and never more than
/// `max_fps` (one frame per `min_frame_interval`). The range is a single
/// rate, so the display ticks at a whole fraction of its refresh.
pub fn preferred_frame_rate(min_frame_interval: Duration, busy: bool) -> FrameRateRange {
    let max_fps = if min_frame_interval.is_zero() {
        BUSY_FRAME_RATE
    } else {
        (1.0 / min_frame_interval.as_secs_f64()) as f32
    };
    let ceiling = if busy {
        BUSY_FRAME_RATE
    } else {
        CALM_FRAME_RATE
    };
    let rate = max_fps.min(ceiling).max(1.0);
    FrameRateRange {
        minimum: rate,
        maximum: rate,
        preferred: rate,
    }
}

/// How many refresh ticks apart frames must be to stay within `max_fps`
/// (one frame per `min_frame_interval`): the smallest whole number of
/// ticks at least that long. Frames then land on vsync at an even cadence:
/// 30 fps on a 60 Hz display is exactly every second tick, never 2/2/3.
pub fn ticks_per_frame(tick_interval: Duration, min_frame_interval: Duration) -> u32 {
    if tick_interval.is_zero() {
        return 1;
    }
    // Slack for timestamp jitter, so 33.3 ms over 16.7 ms ticks is 2, not 3.
    let needed = min_frame_interval.saturating_sub(tick_interval / 8);
    let ticks = needed.as_nanos().div_ceil(tick_interval.as_nanos());
    u32::try_from(ticks).unwrap_or(u32::MAX).max(1)
}

/// Clean ticks in a row before an idle render thread pauses its display
/// link.
pub const IDLE_TICKS_BEFORE_PAUSE: u32 = 4;

/// What to do on a display refresh tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickDecision {
    /// Build and present a frame.
    Frame,
    /// Wait for a later tick.
    Skip,
    /// Nothing has needed drawing for a while: pause the display link.
    Pause,
}

/// Picks the refresh ticks a render thread builds frames on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsyncCadence {
    ticks_since_frame: u32,
    idle_ticks: u32,
}

impl Default for VsyncCadence {
    fn default() -> Self {
        Self {
            // The first dirty tick always draws.
            ticks_since_frame: u32::MAX,
            idle_ticks: 0,
        }
    }
}

impl VsyncCadence {
    /// Decides one tick: a frame when something is `dirty` and at least
    /// `ticks_per_frame` ticks passed since the last frame; a pause after
    /// [`IDLE_TICKS_BEFORE_PAUSE`] clean ticks in a row.
    pub fn on_tick(&mut self, dirty: bool, ticks_per_frame: u32) -> TickDecision {
        self.ticks_since_frame = self.ticks_since_frame.saturating_add(1);
        if !dirty {
            self.idle_ticks = self.idle_ticks.saturating_add(1);
            return if self.idle_ticks >= IDLE_TICKS_BEFORE_PAUSE {
                self.idle_ticks = 0;
                TickDecision::Pause
            } else {
                TickDecision::Skip
            };
        }
        self.idle_ticks = 0;
        if self.ticks_since_frame >= ticks_per_frame {
            self.ticks_since_frame = 0;
            TickDecision::Frame
        } else {
            TickDecision::Skip
        }
    }
}

/// Whether frames are coming in quick succession (output or interaction).
#[derive(Debug, Clone, Copy, Default)]
struct Activity {
    last: Option<Instant>,
    previous: Option<Instant>,
}

impl Activity {
    fn record(&mut self, at: Instant) {
        self.previous = self.last;
        self.last = Some(at);
    }

    /// Two frames within [`BUSY_WINDOW`] of `now`.
    fn busy(&self, now: Instant) -> bool {
        self.previous
            .is_some_and(|previous| now.saturating_duration_since(previous) <= BUSY_WINDOW)
    }
}

/// Counters of a render thread, for tests and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderThreadStats {
    pub frames_built: u64,
    pub frames_presented: u64,
    pub reconfigures: u64,
    pub wakes: u64,
    /// Times the thread paused because its window was occluded.
    pub pauses: u64,
    /// Display refresh ticks received from a [`VsyncSource`].
    pub vsync_ticks: u64,
    /// Times a [`VsyncSource`] was started after a pause.
    pub link_starts: u64,
}

#[derive(Default)]
struct Shared {
    mailbox: SurfaceMailbox,
    /// A frame is wanted. While it stays set, further wakes cost nothing.
    pending: AtomicBool,
    stop: AtomicBool,
    thread: OnceLock<Thread>,
    /// Interrupts a [`VsyncSource`] wait, for a thread paced by one.
    interrupt: OnceLock<Arc<dyn Fn() + Send + Sync>>,
    min_frame_interval_ns: AtomicU64,
    frames_built: AtomicU64,
    frames_presented: AtomicU64,
    reconfigures: AtomicU64,
    wakes: AtomicU64,
    pauses: AtomicU64,
    vsync_ticks: AtomicU64,
    link_starts: AtomicU64,
}

impl Shared {
    fn wake(&self) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.unpark();
        }
    }

    /// Brings the thread out of its wait: a park, or a vsync source's wait.
    fn unpark(&self) {
        if let Some(interrupt) = self.interrupt.get() {
            interrupt();
        }
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }

    fn min_frame_interval(&self) -> Duration {
        Duration::from_nanos(self.min_frame_interval_ns.load(Ordering::Relaxed))
    }
}

/// Asks a render thread for a frame. Cheap, `Send + Sync` and coalescing:
/// wakes before the next frame starts make one frame.
#[derive(Clone)]
pub struct RenderWaker {
    shared: Arc<Shared>,
}

impl RenderWaker {
    pub fn wake(&self) {
        self.shared.wake();
    }
}

impl std::fmt::Debug for RenderWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderWaker").finish_non_exhaustive()
    }
}

/// A window's render thread. Dropping it stops and joins the thread.
pub struct RenderThread {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl RenderThread {
    /// Spawns the thread, which creates its driver with `make_driver` and
    /// draws a first frame for `surface`.
    pub fn spawn<D, F>(
        name: String,
        surface: SurfaceState,
        min_frame_interval: Duration,
        make_driver: F,
    ) -> std::io::Result<Self>
    where
        D: FrameDriver,
        F: FnOnce() -> D + Send + 'static,
    {
        Self::spawn_paced(name, surface, min_frame_interval, move || {
            (make_driver(), None)
        })
    }

    /// Like [`Self::spawn`], but `make` may also return a [`VsyncSource`]
    /// (ft-yccm0.4.1.3): frames are then built only on its refresh ticks.
    /// Without one the thread paces to the frame interval on its own.
    pub fn spawn_paced<D, F>(
        name: String,
        surface: SurfaceState,
        min_frame_interval: Duration,
        make: F,
    ) -> std::io::Result<Self>
    where
        D: FrameDriver,
        F: FnOnce() -> (D, Option<Box<dyn VsyncSource>>) + Send + 'static,
    {
        let shared = Arc::new(Shared {
            mailbox: SurfaceMailbox::new(surface),
            pending: AtomicBool::new(true),
            min_frame_interval_ns: AtomicU64::new(nanos(min_frame_interval)),
            ..Shared::default()
        });
        let thread_shared = Arc::clone(&shared);
        let join = std::thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                let (driver, vsync) = make();
                log::debug!(
                    "render thread {name}: started, paced by {}",
                    if vsync.is_some() {
                        "display refresh"
                    } else {
                        "its frame interval"
                    }
                );
                run(&thread_shared, driver, vsync);
                log::debug!("render thread {name}: stopped");
            })?;
        let _ = shared.thread.set(join.thread().clone());
        // A wake that came before the handle was published left `pending`
        // set; the thread reads it before it first parks.
        join.thread().unpark();
        Ok(Self {
            shared,
            join: Some(join),
        })
    }

    pub fn waker(&self) -> RenderWaker {
        RenderWaker {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Publishes the window's surface and asks for a frame for it. Unlike a
    /// wake, a post always reaches the thread, even while it is paused.
    pub fn post_surface(&self, surface: SurfaceState) {
        self.shared.mailbox.post(surface);
        self.shared.pending.store(true, Ordering::Release);
        self.shared.unpark();
    }

    /// The surface as last posted.
    pub fn surface(&self) -> Surface {
        self.shared.mailbox.read()
    }

    /// The shortest time between two frames: the `max_fps` interval.
    pub fn set_min_frame_interval(&self, interval: Duration) {
        self.shared
            .min_frame_interval_ns
            .store(nanos(interval), Ordering::Relaxed);
    }

    pub fn stats(&self) -> RenderThreadStats {
        let shared = &self.shared;
        RenderThreadStats {
            frames_built: shared.frames_built.load(Ordering::Relaxed),
            frames_presented: shared.frames_presented.load(Ordering::Relaxed),
            reconfigures: shared.reconfigures.load(Ordering::Relaxed),
            wakes: shared.wakes.load(Ordering::Relaxed),
            pauses: shared.pauses.load(Ordering::Relaxed),
            vsync_ticks: shared.vsync_ticks.load(Ordering::Relaxed),
            link_starts: shared.link_starts.load(Ordering::Relaxed),
        }
    }

    /// Whether the thread is still running: it stops only when dropped, or
    /// when its driver panicked.
    pub fn is_running(&self) -> bool {
        self.join.as_ref().is_some_and(|join| !join.is_finished())
    }
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let Some(join) = self.join.take() else {
            return;
        };
        self.shared.unpark();
        join.thread().unpark();
        if join.join().is_err() {
            log::error!("render thread panicked");
        }
    }
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Sets the thread's QoS for `surface` when it changed.
fn follow_qos(qos: &mut Option<ThreadQos>, surface: &Surface) {
    let wanted = surface.state.qos();
    if *qos != Some(wanted) {
        if let Err(err) = procinfo::set_current_thread_qos(wanted) {
            log::debug!("render thread: cannot set QoS {wanted:?}: {err}");
        }
        log::debug!("render thread: QoS {wanted:?}");
        *qos = Some(wanted);
    }
}

/// Builds one frame for `surface`, reconfiguring the driver first when the
/// geometry changed since the last frame (a frame boundary).
fn build_frame<D: FrameDriver>(
    shared: &Shared,
    driver: &mut D,
    configured: &mut Option<(u32, u32, u32)>,
    surface: &Surface,
) -> FrameReport {
    if *configured != Some(surface.state.geometry()) {
        let (width, height, dpi) = surface.state.geometry();
        log::debug!("render thread: surface {width}x{height} at {dpi} dpi");
        driver.reconfigure(surface);
        *configured = Some(surface.state.geometry());
        shared.reconfigures.fetch_add(1, Ordering::Relaxed);
    }
    let report = driver.frame(surface);
    shared.frames_built.fetch_add(1, Ordering::Relaxed);
    if report.presented {
        shared.frames_presented.fetch_add(1, Ordering::Relaxed);
    }
    report
}

fn run<D: FrameDriver>(shared: &Shared, driver: D, vsync: Option<Box<dyn VsyncSource>>) {
    match vsync {
        Some(source) => run_vsync(shared, driver, source),
        None => run_parked(shared, driver),
    }
}

/// The loop of a thread paced by a [`VsyncSource`] (ft-yccm0.4.1.3).
///
/// Frames are built only on display refresh ticks, on the ticks
/// [`VsyncCadence`] picks: at most one per `max_fps` interval, always a whole
/// number of refresh periods apart. A wake or post starts the source and
/// marks the next frame due; a few clean ticks pause it again, so an idle
/// window draws nothing. The rate asked of the display follows
/// [`preferred_frame_rate`].
fn run_vsync<D: FrameDriver>(shared: &Shared, mut driver: D, mut source: Box<dyn VsyncSource>) {
    let _ = shared.interrupt.set(source.interrupter());
    let mut configured: Option<(u32, u32, u32)> = None;
    let mut qos: Option<ThreadQos> = None;
    let mut redraw_at: Option<Instant> = None;
    let mut paused = false;
    let mut running = false;
    let mut cadence = VsyncCadence::default();
    let mut activity = Activity::default();
    let mut rate: Option<FrameRateRange> = None;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let surface = shared.mailbox.read();
        follow_qos(&mut qos, &surface);
        if surface.state.occluded {
            if !paused {
                log::debug!("render thread: paused while occluded");
                shared.pauses.fetch_add(1, Ordering::Relaxed);
                paused = true;
            }
            if running {
                source.set_running(false);
                running = false;
            }
            // Swallow output wakes; only a post (which always interrupts)
            // can make the window visible again.
            shared.pending.store(true, Ordering::Release);
            redraw_at = None;
            source.wait(None);
            continue;
        }
        if paused {
            log::debug!("render thread: resumed");
            paused = false;
        }
        let now = Instant::now();
        let due = |redraw_at: Option<Instant>, now: Instant| redraw_at.is_some_and(|at| at <= now);
        let requested = shared.pending.load(Ordering::Acquire) || due(redraw_at, now);
        if requested && !running {
            source.set_running(true);
            running = true;
            shared.link_starts.fetch_add(1, Ordering::Relaxed);
        }
        let timeout = if running {
            None
        } else {
            redraw_at.map(|at| at.saturating_duration_since(now))
        };
        let tick = match source.wait(timeout) {
            VsyncWait::Tick(tick) => tick,
            VsyncWait::Interrupted | VsyncWait::TimedOut => continue,
        };
        shared.vsync_ticks.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let dirty = shared.pending.load(Ordering::Acquire) || due(redraw_at, now);
        let ticks = ticks_per_frame(tick.interval, shared.min_frame_interval());
        match cadence.on_tick(dirty, ticks) {
            TickDecision::Frame => {}
            TickDecision::Skip => {
                driver.tick_skipped();
                continue;
            }
            TickDecision::Pause => {
                driver.tick_skipped();
                source.set_running(false);
                running = false;
                log::trace!("render thread: display link paused while idle");
                continue;
            }
        }
        // The surface may have changed while waiting for the tick.
        let surface = shared.mailbox.read();
        if surface.state.occluded {
            driver.tick_skipped();
            continue;
        }
        // The frame shows everything before this point; a wake after it
        // asks for the next frame.
        shared.pending.store(false, Ordering::Release);
        if due(redraw_at, now) {
            redraw_at = None;
        }
        let report = build_frame(shared, &mut driver, &mut configured, &surface);
        if let Some(at) = report.redraw_at {
            redraw_at = Some(redraw_at.map_or(at, |due| due.min(at)));
        }
        activity.record(now);
        let wanted = preferred_frame_rate(shared.min_frame_interval(), activity.busy(now));
        if rate != Some(wanted) {
            log::debug!("render thread: frame rate {wanted:?}");
            source.set_rate_range(wanted);
            rate = Some(wanted);
        }
    }
}

/// The loop of a thread without a [`VsyncSource`]: wait for a request (a
/// wake, a post or a due redraw) and for the frame interval to pass, read
/// the surface, follow its QoS, pause while occluded, reconfigure at the
/// frame boundary when the geometry changed, then build one frame.
fn run_parked<D: FrameDriver>(shared: &Shared, mut driver: D) {
    let mut configured: Option<(u32, u32, u32)> = None;
    let mut qos: Option<ThreadQos> = None;
    let mut last_frame: Option<Instant> = None;
    let mut redraw_at: Option<Instant> = None;
    // The generation of the surface that paused the thread.
    let mut paused: Option<u64> = None;
    loop {
        loop {
            if shared.stop.load(Ordering::Acquire) {
                return;
            }
            if let Some(generation) = paused {
                // Occluded: only a post can resume. Output wakes find
                // `pending` set and do not unpark the thread.
                if shared.mailbox.generation() != generation {
                    break;
                }
                std::thread::park();
                continue;
            }
            let now = Instant::now();
            let requested =
                shared.pending.load(Ordering::Acquire) || redraw_at.is_some_and(|due| due <= now);
            let paced = last_frame
                .map(|last| last + shared.min_frame_interval())
                .filter(|due| *due > now);
            match (requested, paced, redraw_at) {
                (true, None, _) => break,
                // Too soon after the last frame. The request stays set, so
                // wakes until then cost nothing.
                (true, Some(due), _) => std::thread::park_timeout(due - now),
                (false, _, Some(due)) => std::thread::park_timeout(due - now),
                (false, _, None) => std::thread::park(),
            }
        }

        let surface = shared.mailbox.read();
        follow_qos(&mut qos, &surface);
        if surface.state.occluded {
            if paused.is_none() {
                log::debug!("render thread: paused while occluded");
                shared.pauses.fetch_add(1, Ordering::Relaxed);
            }
            paused = Some(surface.generation);
            // Swallow output wakes until a post makes the window visible.
            shared.pending.store(true, Ordering::Release);
            redraw_at = None;
            continue;
        }
        if paused.take().is_some() {
            log::debug!("render thread: resumed");
        }
        // The frame below shows everything that happened before this point;
        // a wake after it asks for the next frame.
        shared.pending.store(false, Ordering::Release);
        if redraw_at.is_some_and(|due| due <= Instant::now()) {
            redraw_at = None;
        }

        let report = build_frame(shared, &mut driver, &mut configured, &surface);
        last_frame = Some(Instant::now());
        if let Some(at) = report.redraw_at {
            redraw_at = Some(redraw_at.map_or(at, |due| due.min(at)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Waits up to 10 s for `done`.
    fn eventually(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        done()
    }

    /// A surface whose fields all derive from `n`, so a torn read shows.
    fn correlated(n: u32) -> SurfaceState {
        SurfaceState {
            pixel_width: 100 + n,
            pixel_height: 2 * (100 + n),
            dpi: 72 + n % 256,
            occluded: false,
            focused: n % 2 == 0,
        }
    }

    fn is_correlated(state: &SurfaceState) -> bool {
        let n = state.pixel_width.wrapping_sub(100);
        state.pixel_height == 2 * state.pixel_width
            && state.dpi == 72 + n % 256
            && state.focused == (n % 2 == 0)
    }

    #[derive(Default)]
    struct Record {
        /// The drawable geometry each frame was presented at, with the
        /// geometry it was built for.
        frames: Vec<((u32, u32, u32), (u32, u32, u32))>,
        reconfigured: Vec<SurfaceState>,
        torn: usize,
        qos: Vec<Option<ThreadQos>>,
        /// Each frame's effective base priority (ft-yccm0.6).
        base_priority: Vec<Option<i32>>,
        dropped_on: Option<std::thread::ThreadId>,
    }

    /// Records what the render thread asks of it. Its drawable is sized only
    /// by `reconfigure`, like a CAMetalLayer's drawableSize.
    struct RecordingDriver {
        record: Arc<Mutex<Record>>,
        drawable: (u32, u32, u32),
        frame_time: Duration,
    }

    impl FrameDriver for RecordingDriver {
        fn reconfigure(&mut self, surface: &Surface) {
            self.drawable = surface.state.geometry();
            let mut record = self.record.lock().unwrap();
            record.torn += usize::from(!is_correlated(&surface.state));
            record.reconfigured.push(surface.state);
        }

        fn frame(&mut self, surface: &Surface) -> FrameReport {
            std::thread::sleep(self.frame_time);
            let mut record = self.record.lock().unwrap();
            record.torn += usize::from(!is_correlated(&surface.state));
            record
                .frames
                .push((self.drawable, surface.state.geometry()));
            record.qos.push(procinfo::current_thread_qos());
            record
                .base_priority
                .push(procinfo::current_thread_base_priority());
            FrameReport {
                presented: true,
                redraw_at: None,
            }
        }
    }

    impl Drop for RecordingDriver {
        fn drop(&mut self) {
            self.record.lock().unwrap().dropped_on = Some(std::thread::current().id());
        }
    }

    fn spawn_recording(
        surface: SurfaceState,
        interval: Duration,
        frame_time: Duration,
    ) -> (RenderThread, Arc<Mutex<Record>>) {
        let record = Arc::new(Mutex::new(Record::default()));
        let driver_record = Arc::clone(&record);
        let thread =
            RenderThread::spawn("ft-render-test".to_string(), surface, interval, move || {
                RecordingDriver {
                    record: driver_record,
                    drawable: (0, 0, 0),
                    frame_time,
                }
            })
            .unwrap();
        (thread, record)
    }

    #[test]
    fn the_mailbox_never_returns_a_torn_surface() {
        let mailbox = SurfaceMailbox::new(correlated(0));
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for n in 1..=200_000 {
                    mailbox.post(correlated(n));
                }
                done.store(true, Ordering::Release);
            });
            let mut last = 0;
            while !done.load(Ordering::Acquire) {
                let surface = mailbox.read();
                assert!(is_correlated(&surface.state), "torn {:?}", surface.state);
                assert!(surface.generation >= last, "generations only grow");
                last = surface.generation;
            }
        });
        let last = mailbox.read();
        assert_eq!(last.state, correlated(200_000));
        assert_eq!(last.generation, 200_001);
    }

    /// The acceptance test: frames keep being presented while the main
    /// thread is blocked for 200 ms. Pane output wakes the render thread
    /// from another thread, and nothing waits for the main thread.
    #[test]
    fn frames_keep_presenting_while_the_main_thread_is_blocked() {
        let (thread, _record) =
            spawn_recording(correlated(0), Duration::from_millis(4), Duration::ZERO);
        let waker = thread.waker();
        let output_done = AtomicBool::new(false);
        let (before, after) = std::thread::scope(|scope| {
            // A parse thread delivering output every millisecond.
            scope.spawn(|| {
                while !output_done.load(Ordering::Acquire) {
                    waker.wake();
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            assert!(eventually(|| thread.stats().frames_presented > 0));
            let before = thread.stats();
            // The main thread is blocked: no posts, no wakes, no tasks.
            std::thread::sleep(Duration::from_millis(200));
            let after = thread.stats();
            output_done.store(true, Ordering::Release);
            (before, after)
        });
        let presented = after.frames_presented - before.frames_presented;
        // At most 50 at the 4 ms interval; honest slack for a loaded host.
        assert!(
            presented >= 10,
            "{presented} frames presented while the main thread was blocked for 200 ms"
        );
        assert!(
            presented <= 52,
            "{presented} frames in 200 ms break the 4 ms pacing"
        );
    }

    /// A resize storm: thousands of surface posts while output keeps waking
    /// the thread. Every frame is built for the drawable size it is
    /// presented at (no stretched frame), no read is torn, the thread
    /// reconfigures far less often than it was posted to, and it settles at
    /// the last size.
    #[test]
    fn a_resize_storm_presents_every_frame_at_its_own_size_and_settles_on_the_last() {
        let (thread, record) = spawn_recording(
            correlated(0),
            Duration::from_millis(2),
            Duration::from_micros(300),
        );
        let waker = thread.waker();
        const POSTS: u32 = 5_000;
        let storm_done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let storm_done = &storm_done;
            scope.spawn(|| {
                while !storm_done.load(Ordering::Acquire) {
                    waker.wake();
                    std::thread::yield_now();
                }
            });
            for n in 1..=POSTS {
                thread.post_surface(correlated(n));
                if n % 50 == 0 {
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
            storm_done.store(true, Ordering::Release);
        });
        let last = correlated(POSTS).geometry();
        assert!(
            eventually(|| record
                .lock()
                .unwrap()
                .frames
                .last()
                .is_some_and(|(drawable, _)| *drawable == last)),
            "the thread never presented the last size"
        );
        let record = record.lock().unwrap();
        assert_eq!(record.torn, 0, "a torn surface reached the driver");
        for (drawable, built) in &record.frames {
            assert_eq!(
                drawable, built,
                "a frame presented at a size it was not built for"
            );
        }
        let stats = thread.stats();
        assert!(stats.reconfigures >= 2);
        assert!(
            stats.reconfigures < u64::from(POSTS),
            "{} reconfigures for {POSTS} posts: posts between frames must coalesce",
            stats.reconfigures
        );
    }

    #[test]
    fn dropping_the_render_thread_joins_it_after_dropping_the_driver_there() {
        let (thread, record) =
            spawn_recording(correlated(0), Duration::from_millis(1), Duration::ZERO);
        assert!(eventually(|| thread.stats().frames_presented > 0));
        let render_thread_id = thread.join.as_ref().unwrap().thread().id();
        assert!(thread.is_running());
        drop(thread);
        let dropped_on = record.lock().unwrap().dropped_on;
        assert_eq!(dropped_on, Some(render_thread_id));
        assert_ne!(dropped_on, Some(std::thread::current().id()));
    }

    #[test]
    fn an_occluded_window_pauses_frames_until_it_is_visible_again() {
        let (thread, _record) =
            spawn_recording(correlated(0), Duration::from_millis(1), Duration::ZERO);
        assert!(eventually(|| thread.stats().frames_presented > 0));
        let occluded = SurfaceState {
            occluded: true,
            ..correlated(0)
        };
        thread.post_surface(occluded);
        assert!(eventually(|| thread.stats().pauses == 1));
        let paused = thread.stats();
        let waker = thread.waker();
        for _ in 0..50 {
            waker.wake();
            std::thread::sleep(Duration::from_millis(1));
        }
        // A focus change while occluded is read, and the thread stays paused.
        thread.post_surface(SurfaceState {
            focused: false,
            ..occluded
        });
        std::thread::sleep(Duration::from_millis(20));
        let still = thread.stats();
        assert_eq!(still.wakes, paused.wakes + 50);
        assert_eq!(still.pauses, 1);
        assert_eq!(
            still.frames_built, paused.frames_built,
            "an occluded window built a frame"
        );
        thread.post_surface(correlated(0));
        assert!(eventually(
            || thread.stats().frames_built > paused.frames_built
        ));
    }

    /// The class follows focus, and it is the priority the thread actually
    /// runs at, not only the class it asked for: in a process without an
    /// application task role (a test binary, a GUI started from a shell)
    /// macOS would run both at the default 31 (ft-yccm0.6).
    #[test]
    fn the_thread_qos_follows_focus() {
        let (thread, record) =
            spawn_recording(correlated(0), Duration::from_millis(1), Duration::ZERO);
        let last_frame = || {
            let record = record.lock().unwrap();
            (
                record.qos.last().copied().flatten(),
                record.base_priority.last().copied().flatten(),
            )
        };
        let expect = |qos: ThreadQos| {
            if cfg!(target_os = "macos") {
                (Some(qos), Some(qos.base_priority()))
            } else {
                (None, None)
            }
        };
        assert!(correlated(0).focused);
        assert!(eventually(|| last_frame() == expect(FOCUSED_QOS)));
        thread.post_surface(correlated(1));
        assert!(!correlated(1).focused);
        assert!(eventually(|| last_frame() == expect(UNFOCUSED_QOS)));
        thread.post_surface(correlated(2));
        assert!(eventually(|| last_frame() == expect(FOCUSED_QOS)));
    }

    #[test]
    fn wakes_faster_than_the_frame_interval_coalesce() {
        let (thread, _record) =
            spawn_recording(correlated(0), Duration::from_millis(20), Duration::ZERO);
        assert!(eventually(|| thread.stats().frames_presented > 0));
        let before = thread.stats();
        let started = Instant::now();
        let waker = thread.waker();
        while started.elapsed() < Duration::from_millis(200) {
            waker.wake();
            std::thread::sleep(Duration::from_micros(100));
        }
        let elapsed = started.elapsed();
        let frames = thread.stats().frames_built - before.frames_built;
        let allowed = elapsed.as_millis() / 20 + 2;
        assert!(
            u128::from(frames) <= allowed,
            "{frames} frames in {elapsed:?} at a 20 ms interval"
        );
        assert!(frames >= 2, "wakes produced {frames} frames");
    }

    // ft-yccm0.4.1.3: display-link pacing, with a fake link the test ticks.

    fn hz(rate: f64) -> Duration {
        Duration::from_secs_f64(1.0 / rate)
    }

    #[test]
    fn ticks_per_frame_is_the_whole_number_of_refresh_periods_max_fps_allows() {
        for (refresh, max_fps, ticks) in [
            (60.0, 30.0, 2),
            (60.0, 60.0, 1),
            (60.0, 120.0, 1),
            (120.0, 30.0, 4),
            (120.0, 60.0, 2),
            (120.0, 120.0, 1),
            // 50 fps fits no whole number of 60 Hz periods: the bound wins.
            (60.0, 50.0, 2),
            (120.0, 50.0, 3),
        ] {
            assert_eq!(
                ticks_per_frame(hz(refresh), hz(max_fps)),
                ticks,
                "{max_fps} fps at {refresh} Hz"
            );
        }
        assert_eq!(ticks_per_frame(Duration::ZERO, hz(60.0)), 1);
    }

    #[test]
    fn the_cadence_draws_every_nth_dirty_tick_and_pauses_after_clean_ones() {
        let mut cadence = VsyncCadence::default();
        let decisions: Vec<_> = (0..6).map(|_| cadence.on_tick(true, 2)).collect();
        use TickDecision::{Frame, Pause, Skip};
        assert_eq!(decisions, [Frame, Skip, Frame, Skip, Frame, Skip]);
        let clean: Vec<_> = (0..IDLE_TICKS_BEFORE_PAUSE)
            .map(|_| cadence.on_tick(false, 2))
            .collect();
        assert_eq!(clean.last(), Some(&Pause));
        assert!(
            clean[..clean.len() - 1]
                .iter()
                .all(|decision| *decision == Skip)
        );
        // Dirty again after the pause: the first tick draws at once.
        assert_eq!(cadence.on_tick(true, 2), Frame);
    }

    #[test]
    fn the_rate_asked_of_the_display_follows_activity_under_max_fps() {
        let rate = |max_fps: f64, busy: bool| preferred_frame_rate(hz(max_fps), busy).preferred;
        assert_eq!(rate(120.0, true), 120.0);
        assert_eq!(rate(120.0, false), 60.0);
        assert_eq!(rate(60.0, true), 60.0);
        assert_eq!(rate(30.0, true), 30.0);
        assert_eq!(rate(30.0, false), 30.0);
        let range = preferred_frame_rate(hz(30.0), true);
        assert_eq!((range.minimum, range.maximum), (30.0, 30.0));

        let start = Instant::now();
        let mut activity = Activity::default();
        activity.record(start);
        assert!(!activity.busy(start), "one frame is not busy");
        activity.record(start + Duration::from_millis(100));
        assert!(activity.busy(start + Duration::from_millis(100)));
        assert!(!activity.busy(start + Duration::from_millis(400)));
    }

    enum LinkMessage {
        Tick(Duration),
        Interrupt,
    }

    #[derive(Default)]
    struct LinkLog {
        running: Vec<bool>,
        ranges: Vec<FrameRateRange>,
    }

    /// A display link the test ticks by hand. A paused link drops ticks,
    /// as a real one does not deliver them.
    struct FakeLink {
        rx: std::sync::mpsc::Receiver<LinkMessage>,
        tx: std::sync::mpsc::Sender<LinkMessage>,
        running: bool,
        returned: u64,
        /// The ticks returned before the thread last entered `wait`.
        settled: Arc<AtomicU64>,
        log: Arc<Mutex<LinkLog>>,
    }

    impl VsyncSource for FakeLink {
        fn wait(&mut self, timeout: Option<Duration>) -> VsyncWait {
            self.settled.store(self.returned, Ordering::Release);
            let deadline = timeout.map(|timeout| Instant::now() + timeout);
            loop {
                let message = match deadline {
                    Some(deadline) => match self
                        .rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(message) => message,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            return VsyncWait::TimedOut;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            return VsyncWait::Interrupted;
                        }
                    },
                    None => match self.rx.recv() {
                        Ok(message) => message,
                        Err(_) => return VsyncWait::Interrupted,
                    },
                };
                match message {
                    LinkMessage::Interrupt => return VsyncWait::Interrupted,
                    LinkMessage::Tick(_) if !self.running => continue,
                    LinkMessage::Tick(interval) => {
                        self.returned += 1;
                        return VsyncWait::Tick(VsyncTick { interval });
                    }
                }
            }
        }

        fn set_running(&mut self, running: bool) {
            self.running = running;
            self.log.lock().unwrap().running.push(running);
        }

        fn set_rate_range(&mut self, range: FrameRateRange) {
            self.log.lock().unwrap().ranges.push(range);
        }

        fn interrupter(&self) -> Arc<dyn Fn() + Send + Sync> {
            let tx = self.tx.clone();
            Arc::new(move || {
                let _ = tx.send(LinkMessage::Interrupt);
            })
        }
    }

    struct LinkHandle {
        tx: std::sync::mpsc::Sender<LinkMessage>,
        settled: Arc<AtomicU64>,
        expected: std::cell::Cell<u64>,
        log: Arc<Mutex<LinkLog>>,
    }

    impl LinkHandle {
        /// A tick of a running link; returns once the thread has drawn or
        /// skipped it and is waiting again.
        fn tick(&self, interval: Duration) {
            self.expected.set(self.expected.get() + 1);
            self.tx.send(LinkMessage::Tick(interval)).unwrap();
            let expected = self.expected.get();
            assert!(
                eventually(|| self.settled.load(Ordering::Acquire) >= expected),
                "the render thread never dealt with tick {expected}"
            );
        }

        /// A tick sent to a link the thread has paused: it is dropped.
        fn tick_while_paused(&self, interval: Duration) {
            self.tx.send(LinkMessage::Tick(interval)).unwrap();
        }

        fn running(&self) -> Vec<bool> {
            self.log.lock().unwrap().running.clone()
        }
    }

    /// Always dirty: asks to be drawn again at once.
    struct DirtyDriver(bool);

    impl FrameDriver for DirtyDriver {
        fn reconfigure(&mut self, _surface: &Surface) {}

        fn frame(&mut self, _surface: &Surface) -> FrameReport {
            FrameReport {
                presented: true,
                redraw_at: self.0.then(Instant::now),
            }
        }
    }

    fn spawn_with_fake_link(max_fps: f64, always_dirty: bool) -> (RenderThread, LinkHandle) {
        let (tx, rx) = std::sync::mpsc::channel();
        let settled = Arc::new(AtomicU64::new(0));
        let log = Arc::new(Mutex::new(LinkLog::default()));
        let link = FakeLink {
            rx,
            tx: tx.clone(),
            running: false,
            returned: 0,
            settled: Arc::clone(&settled),
            log: Arc::clone(&log),
        };
        let thread = RenderThread::spawn_paced(
            "ft-render-link-test".to_string(),
            correlated(0),
            hz(max_fps),
            move || {
                (
                    DirtyDriver(always_dirty),
                    Some(Box::new(link) as Box<dyn VsyncSource>),
                )
            },
        )
        .unwrap();
        let handle = LinkHandle {
            tx,
            settled,
            expected: std::cell::Cell::new(0),
            log,
        };
        // The first frame is due at once, so the thread starts its link.
        assert!(eventually(|| handle.running() == [true]));
        (thread, handle)
    }

    /// The ticks, numbered from 1, on which frames were built while the
    /// window stayed dirty.
    fn frame_ticks(refresh: f64, max_fps: f64, ticks: u64) -> Vec<u64> {
        let (thread, link) = spawn_with_fake_link(max_fps, true);
        let mut drawn = Vec::new();
        for tick in 1..=ticks {
            let before = thread.stats().frames_built;
            link.tick(hz(refresh));
            if thread.stats().frames_built > before {
                drawn.push(tick);
            }
        }
        assert_eq!(thread.stats().vsync_ticks, ticks);
        drawn
    }

    /// The acceptance test for max_fps: frames land on every Nth refresh
    /// tick exactly, at 60 and 120 Hz, never with 2/2/3 judder.
    #[test]
    fn max_fps_frames_land_on_every_nth_refresh_tick() {
        for (refresh, max_fps, every) in [
            (60.0, 30.0, 2),
            (60.0, 60.0, 1),
            (60.0, 120.0, 1),
            (120.0, 30.0, 4),
            (120.0, 60.0, 2),
            (120.0, 120.0, 1),
        ] {
            let expected: Vec<u64> = (1..=24).step_by(every).collect();
            assert_eq!(
                frame_ticks(refresh, max_fps, 24),
                expected,
                "{max_fps} fps at {refresh} Hz"
            );
        }
    }

    /// An idle window pauses its link after a few clean ticks and draws
    /// nothing; a wake starts the link again, and the frame waits for its
    /// next tick: frames are built only on ticks.
    #[test]
    fn an_idle_window_pauses_its_link_and_draws_only_on_ticks() {
        let (thread, link) = spawn_with_fake_link(60.0, false);
        link.tick(hz(60.0));
        assert_eq!(thread.stats().frames_built, 1, "the first tick draws");
        for _ in 0..IDLE_TICKS_BEFORE_PAUSE {
            link.tick(hz(60.0));
        }
        assert_eq!(link.running(), [true, false], "clean ticks pause the link");
        for _ in 0..10 {
            link.tick_while_paused(hz(60.0));
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(thread.stats().frames_built, 1);

        thread.waker().wake();
        assert!(eventually(|| link.running() == [true, false, true]));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            thread.stats().frames_built,
            1,
            "a wake starts the link but draws nothing before its tick"
        );
        link.tick(hz(60.0));
        assert_eq!(thread.stats().frames_built, 2);
        assert_eq!(thread.stats().link_starts, 2);
    }

    /// Busy output asks a 120 Hz display for its full rate when max_fps
    /// allows; the rate stays within max_fps.
    #[test]
    fn busy_frames_ask_the_display_for_max_fps() {
        let (_thread, link) = spawn_with_fake_link(120.0, true);
        for _ in 0..6 {
            link.tick(hz(120.0));
        }
        let ranges = link.log.lock().unwrap().ranges.clone();
        assert_eq!(ranges.last().map(|range| range.preferred), Some(120.0));
        assert!(ranges.iter().all(|range| range.preferred <= 120.0));

        let (_thread, link) = spawn_with_fake_link(30.0, true);
        for _ in 0..8 {
            link.tick(hz(60.0));
        }
        let ranges = link.log.lock().unwrap().ranges.clone();
        assert!(ranges.iter().all(|range| range.preferred == 30.0));
    }
}
