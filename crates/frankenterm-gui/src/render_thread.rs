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
}

#[derive(Default)]
struct Shared {
    mailbox: SurfaceMailbox,
    /// A frame is wanted. While it stays set, further wakes cost nothing.
    pending: AtomicBool,
    stop: AtomicBool,
    thread: OnceLock<Thread>,
    min_frame_interval_ns: AtomicU64,
    frames_built: AtomicU64,
    frames_presented: AtomicU64,
    reconfigures: AtomicU64,
    wakes: AtomicU64,
    pauses: AtomicU64,
}

impl Shared {
    fn wake(&self) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.unpark();
        }
    }

    fn unpark(&self) {
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
                log::debug!("render thread {name}: started");
                run(&thread_shared, make_driver());
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
        join.thread().unpark();
        if join.join().is_err() {
            log::error!("render thread panicked");
        }
    }
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// The thread's loop: wait for a request (a wake, a post or a due redraw)
/// and for the frame interval to pass, read the surface, follow its QoS,
/// pause while occluded, reconfigure at the frame boundary when the
/// geometry changed, then build one frame.
fn run<D: FrameDriver>(shared: &Shared, mut driver: D) {
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
        let wanted = surface.state.qos();
        if qos != Some(wanted) {
            if let Err(err) = procinfo::set_current_thread_qos(wanted) {
                log::debug!("render thread: cannot set QoS {wanted:?}: {err}");
            }
            log::debug!("render thread: QoS {wanted:?}");
            qos = Some(wanted);
        }
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

        if configured != Some(surface.state.geometry()) {
            let (width, height, dpi) = surface.state.geometry();
            log::debug!("render thread: surface {width}x{height} at {dpi} dpi");
            driver.reconfigure(&surface);
            configured = Some(surface.state.geometry());
            shared.reconfigures.fetch_add(1, Ordering::Relaxed);
        }
        let report = driver.frame(&surface);
        last_frame = Some(Instant::now());
        shared.frames_built.fetch_add(1, Ordering::Relaxed);
        if report.presented {
            shared.frames_presented.fetch_add(1, Ordering::Relaxed);
        }
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

    #[test]
    fn the_thread_qos_follows_focus() {
        let (thread, record) =
            spawn_recording(correlated(0), Duration::from_millis(1), Duration::ZERO);
        let qos_of_last_frame = || record.lock().unwrap().qos.last().copied().flatten();
        let expect = |qos: ThreadQos| cfg!(target_os = "macos").then_some(qos);
        assert!(correlated(0).focused);
        assert!(eventually(|| qos_of_last_frame() == expect(FOCUSED_QOS)));
        thread.post_surface(correlated(1));
        assert!(!correlated(1).focused);
        assert!(eventually(|| qos_of_last_frame() == expect(UNFOCUSED_QOS)));
        thread.post_surface(correlated(2));
        assert!(eventually(|| qos_of_last_frame() == expect(FOCUSED_QOS)));
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
}
