//! The Metal front end's render thread (ft-yccm0.4.1.2).
//!
//! With `front_end = "Metal"`, the window's renderer is attached on the main
//! thread, as AppKit requires, then moved to a render thread of its own
//! ([`frankenterm_gui::render_thread`]). From then on the render thread
//! captures every visible pane's changed rows, rebuilds their scenes and
//! glyphs, and encodes and presents every frame: all the panes in one render
//! pass (ft-yccm0.4.6). The main thread only publishes what a frame needs
//! from the window ([`MetalFrameRequest`], when it would have painted) and
//! the window's surface (size, DPI, focus) through the render thread's
//! lock-free mailbox. Output of any pane being drawn wakes the render thread
//! straight from the thread that delivers the mux notification
//! ([`MetalOutputWake`]), with no main-thread task per batch.
//!
//! The render thread keeps its own fonts, built for the window's DPI and
//! font scale, since a `FontConfiguration` cannot leave the main thread.
//!
//! On macOS 14 and later the render thread is paced by the window's
//! `CAMetalDisplayLink` (ft-yccm0.4.1.3): frames are encoded only in its
//! refresh callbacks, into the drawable each callback hands over, on the
//! ticks `render_thread::VsyncCadence` picks under `max_fps`; the link is
//! paused while nothing changes. Earlier macOS paces to the `max_fps`
//! interval instead.

use super::TermWindow;
use super::metal_chrome::ChromeFrame;
use super::metal_window::{MetalPaneRequest, MetalPanes};
use crate::utilsprites::RenderMetrics;
use config::ConfigHandle;
use frankenterm_alloc::resource_ledger::FrameLedger;
use frankenterm_font::FontConfiguration;
#[cfg(test)]
use frankenterm_gui::render_thread::RenderThreadStats;
use frankenterm_gui::render_thread::{
    FrameDriver, FrameRateRange, FrameReport, RenderThread, RenderWaker, Surface, SurfaceState,
    VsyncSource, VsyncTick, VsyncWait,
};
use frankenterm_renderer_metal::{
    ClearColor, FrameOutcome, GridExtent, LinkUpdate, LinkWait, MetalDisplayLink, MetalRenderer,
    MetalRendererHandoff, SolidRect,
};
use mux::pane::PaneId;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

/// `FRANKENTERM_METAL_RENDER_THREAD=0` keeps Metal frames on the main thread.
pub(crate) const RENDER_THREAD_ENV: &str = "FRANKENTERM_METAL_RENDER_THREAD";

/// Whether Metal frames should be drawn on a render thread: the default,
/// unless [`RENDER_THREAD_ENV`] is `0`.
pub(crate) fn render_thread_enabled() -> bool {
    std::env::var_os(RENDER_THREAD_ENV).is_none_or(|value| value != "0")
}

/// `FRANKENTERM_METAL_DISPLAY_LINK=0` paces the render thread to the
/// `max_fps` interval instead of the display link.
pub(crate) const DISPLAY_LINK_ENV: &str = "FRANKENTERM_METAL_DISPLAY_LINK";

/// How a render thread is paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pacing {
    /// The window's display link when the OS has one (ft-yccm0.4.1.3).
    DisplayLink,
    /// The `max_fps` interval.
    Interval,
}

impl Pacing {
    /// The display link unless [`DISPLAY_LINK_ENV`] is `0`.
    pub(crate) fn from_env() -> Self {
        if std::env::var_os(DISPLAY_LINK_ENV).is_some_and(|value| value == "0") {
            Self::Interval
        } else {
            Self::DisplayLink
        }
    }
}

/// What the main thread publishes for the render thread's next frame.
#[derive(Clone)]
pub(crate) struct MetalFrameRequest {
    /// The visible panes (ft-yccm0.4.6).
    pub(crate) panes: Vec<MetalPaneRequest>,
    /// Split borders.
    pub(crate) splits: Vec<SolidRect>,
    /// The fancy tab bar and modal overlays, laid out and painted on the
    /// main thread (ft-yccm0.4.7.1).
    pub(crate) chrome: Option<ChromeFrame>,
    pub(crate) config: ConfigHandle,
    /// What shows where no pane draws.
    pub(crate) clear: ClearColor,
    /// The grid a frame without a pane clears.
    pub(crate) grid: GridExtent,
    pub(crate) font_scale: f64,
    /// The window background is opaque, so the layer can be too: only a
    /// configured transparency or background image makes it non-opaque.
    pub(crate) opaque: bool,
}

/// How many recent refresh periods the link's period estimate looks back.
const PERIOD_SAMPLES: usize = 8;

/// The window's display link as the render thread's [`VsyncSource`]
/// (ft-yccm0.4.1.3). Each update's drawable goes to the driver through
/// `update`.
struct LinkSource {
    link: MetalDisplayLink,
    update: Rc<RefCell<Option<LinkUpdate>>>,
    last_target: Option<f64>,
    /// Recent periods between consecutive updates, newest last.
    periods: Vec<Duration>,
}

impl LinkSource {
    /// The display's refresh period: the shortest recent one, so a dropped
    /// tick does not halve the estimate, which still follows a rate change
    /// within [`PERIOD_SAMPLES`] ticks. 60 Hz until there is a sample.
    fn period(&self) -> Duration {
        self.periods
            .iter()
            .min()
            .copied()
            .unwrap_or(Duration::from_nanos(16_666_667))
    }
}

impl VsyncSource for LinkSource {
    fn wait(&mut self, timeout: Option<Duration>) -> VsyncWait {
        match self.link.wait(timeout) {
            LinkWait::Update(update) => {
                let target = update.target_presentation_timestamp();
                if let Some(last) = self.last_target {
                    let period = target - last;
                    if period > 0.0 && period < 0.25 {
                        if self.periods.len() == PERIOD_SAMPLES {
                            self.periods.remove(0);
                        }
                        self.periods.push(Duration::from_secs_f64(period));
                    }
                }
                self.last_target = Some(target);
                *self.update.borrow_mut() = Some(update);
                VsyncWait::Tick(VsyncTick {
                    interval: self.period(),
                })
            }
            LinkWait::Interrupted => VsyncWait::Interrupted,
            LinkWait::TimedOut => VsyncWait::TimedOut,
        }
    }

    fn set_running(&mut self, running: bool) {
        self.link.set_paused(!running);
        if !running {
            // The next update comes after a gap, not one period later.
            self.last_target = None;
        }
    }

    fn set_rate_range(&mut self, range: FrameRateRange) {
        self.link
            .set_frame_rate(range.minimum, range.maximum, range.preferred);
    }

    fn interrupter(&self) -> Arc<dyn Fn() + Send + Sync> {
        let interrupter = self.link.interrupter();
        Arc::new(move || interrupter.interrupt())
    }
}

/// Wakes a window's render thread for the output of a pane it draws, on the
/// thread that delivers the mux notification (ft-yccm0.4.1.2).
#[derive(Debug, Default)]
pub(crate) struct MetalOutputWake {
    waker: Mutex<Option<RenderWaker>>,
    /// The panes the render thread draws: every visible pane, active or not
    /// (ft-yccm0.4.6), and the pane under an overlay.
    panes: RwLock<Vec<PaneId>>,
}

impl MetalOutputWake {
    /// Wakes the render thread when it draws `pane_id`. False when there is
    /// no render thread or it does not draw that pane, so the caller takes
    /// the ordinary main-thread path.
    pub(crate) fn wake_for(&self, pane_id: PaneId) -> bool {
        if !self
            .panes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&pane_id)
        {
            return false;
        }
        let waker = self.waker.lock().unwrap_or_else(PoisonError::into_inner);
        match waker.as_ref() {
            Some(waker) => {
                waker.wake();
                true
            }
            None => false,
        }
    }

    fn set_waker(&self, waker: Option<RenderWaker>) {
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = waker;
    }

    fn set_panes(&self, panes: Vec<PaneId>) {
        let mut current = self.panes.write().unwrap_or_else(PoisonError::into_inner);
        if *current != panes {
            *current = panes;
        }
    }
}

/// The render thread's fonts, built for one configuration, DPI and font
/// scale.
struct RenderFonts {
    key: (usize, u32, u64),
    fonts: Rc<FontConfiguration>,
    metrics: RenderMetrics,
}

/// Draws a window's Metal frames on its render thread.
struct MetalDriver {
    request: Arc<Mutex<Arc<MetalFrameRequest>>>,
    panes: MetalPanes,
    fonts: Option<RenderFonts>,
    /// The drawable size of the last presented frame, width in the high 32
    /// bits: what the window shows now.
    presented_size: Arc<AtomicU64>,
    /// The display link's latest update, when the link paces this thread:
    /// its drawable is the only one a frame is drawn into.
    link_update: Rc<RefCell<Option<LinkUpdate>>>,
    paced_by_link: bool,
    /// The layer opacity last applied.
    opaque: Option<bool>,
    /// Rows rebuilt per pane since the thread started.
    #[cfg(test)]
    rows_rebuilt: Arc<Mutex<std::collections::HashMap<PaneId, u64>>>,
    // Last: off macOS the renderer is uninhabited, and fields built after it
    // would never be evaluated.
    renderer: Rc<MetalRenderer>,
}

impl MetalDriver {
    /// Fonts for `request` at `dpi`, rebuilt only when the configuration,
    /// DPI or font scale changed.
    fn fonts(&mut self, request: &MetalFrameRequest, dpi: u32) -> anyhow::Result<&RenderFonts> {
        let key = (
            request.config.generation(),
            dpi,
            request.font_scale.to_bits(),
        );
        if self.fonts.as_ref().is_none_or(|fonts| fonts.key != key) {
            let dpi_usize = usize::try_from(dpi)?;
            let fonts = Rc::new(FontConfiguration::new(
                Some(request.config.clone()),
                dpi_usize,
            )?);
            if request.font_scale != 1.0 {
                fonts.change_scaling(request.font_scale, dpi_usize);
            }
            let metrics = RenderMetrics::new(&fonts)?;
            log::debug!(
                "render thread: fonts for config {} at {dpi} dpi, scale {}",
                key.0,
                request.font_scale
            );
            self.fonts = Some(RenderFonts {
                key,
                fonts,
                metrics,
            });
        }
        self.fonts
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("render thread fonts are missing"))
    }
}

impl FrameDriver for MetalDriver {
    fn reconfigure(&mut self, surface: &Surface) {
        // The next frame sizes the drawable to the surface (inside an
        // explicit CATransaction); fonts follow the DPI in `frame`. The
        // contents scale follows the new drawable size here (ft-yccm0.4.1.3).
        let (width, height, dpi) = surface.state.geometry();
        log::debug!("render thread: drawable {width}x{height} at {dpi} dpi");
        let opaque = self
            .request
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .opaque;
        self.renderer.configure_surface(width, opaque);
        self.opaque = Some(opaque);
    }

    fn tick_skipped(&mut self) {
        // Release the skipped refresh's drawable to the layer's pool.
        self.link_update.borrow_mut().take();
    }

    fn frame(&mut self, surface: &Surface) -> FrameReport {
        let request = Arc::clone(&self.request.lock().unwrap_or_else(PoisonError::into_inner));
        let (width, height, dpi) = surface.state.geometry();
        if self.opaque != Some(request.opaque) {
            self.renderer.configure_surface(width, request.opaque);
            self.opaque = Some(request.opaque);
        }
        let update = if self.paced_by_link {
            // Paced by the link, a frame is drawn only into the drawable of
            // the refresh that called for it.
            match self.link_update.borrow_mut().take() {
                Some(update) => Some(update),
                None => return FrameReport::default(),
            }
        } else {
            None
        };
        let (fonts, metrics) = match self.fonts(&request, dpi) {
            Ok(fonts) => (Rc::clone(&fonts.fonts), fonts.metrics.clone()),
            Err(err) => {
                log::error!("render thread: cannot load fonts: {err:#}");
                return FrameReport::default();
            }
        };
        // Every visible pane in one render pass (ft-yccm0.4.6).
        let renderer = Rc::clone(&self.renderer);
        let outcome = self.panes.draw(
            &request.panes,
            &request.splits,
            request.chrome.as_ref().map(ChromeFrame::layer),
            request.clear,
            &fonts,
            &metrics,
            &renderer,
            |window| match (window, update) {
                (Some(window), Some(update)) => {
                    renderer.render_window_into(update, width, height, window)
                }
                (Some(window), None) => renderer.render_window(width, height, window),
                (None, Some(update)) => {
                    renderer.render_clear_into(update, width, height, request.grid, request.clear)
                }
                (None, None) => renderer.render_clear(width, height, request.grid, request.clear),
            },
        );
        let changed = self
            .panes
            .rows_rebuilt()
            .iter()
            .filter(|(_, rows)| *rows > 0)
            .count();
        metrics::histogram!("gui.metal.window.panes_rebuilt").record(changed as f64);
        #[cfg(test)]
        {
            let mut counts = self
                .rows_rebuilt
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (pane, rows) in self.panes.rows_rebuilt() {
                *counts.entry(*pane).or_default() += *rows as u64;
            }
        }
        match outcome {
            Ok(FrameOutcome::Presented) => {
                let (width, height) = self.renderer.drawable_size();
                self.presented_size.store(
                    (u64::from(width) << 32) | u64::from(height),
                    Ordering::Relaxed,
                );
                // The GUI's own presented-frame count and cap, which the
                // throughput harness checks its screen-capture FPS meter
                // against (ft-yccm0.1.4), as the WebGpu paint records them;
                // nv9 read 0 presents/s from a Metal window without this.
                let frames = FrameLedger::global();
                frames.set_max_fps(request.config.max_fps);
                frames.record_present();
                // Blinking text or a blinking cursor draws again when its
                // blink next moves (ft-yccm0.4.7.3).
                FrameReport {
                    presented: true,
                    redraw_at: self.panes.redraw_at(),
                }
            }
            Ok(FrameOutcome::ZeroSize) => FrameReport::default(),
            // The link's drawable predates a resize: draw on the next tick,
            // whose drawable has the new size.
            Ok(FrameOutcome::DrawableResized) => FrameReport {
                presented: false,
                redraw_at: Some(Instant::now()),
            },
            Err(err) => {
                // A drawable or frame slot that stayed busy past its wait:
                // try again at the next frame the interval allows.
                metrics::counter!("gui.metal.render_thread.frame_failed").increment(1);
                FrameLedger::global().record_present_failure();
                log::warn!("render thread: Metal frame failed: {err:#}");
                FrameReport {
                    presented: false,
                    redraw_at: Some(Instant::now()),
                }
            }
        }
    }
}

/// The main thread's handle on a window's Metal render thread. Dropping it
/// stops the output wakes, then stops and joins the thread.
pub(crate) struct MetalRenderThread {
    thread: RenderThread,
    request: Arc<Mutex<Arc<MetalFrameRequest>>>,
    output: Arc<MetalOutputWake>,
    surface: std::cell::Cell<SurfaceState>,
    #[cfg(test)]
    presented_size: Arc<AtomicU64>,
    #[cfg(test)]
    rows_rebuilt: Arc<Mutex<std::collections::HashMap<PaneId, u64>>>,
}

impl MetalRenderThread {
    /// Moves `renderer` to a new render thread that draws `request` on
    /// `surface`, at most once per `min_frame_interval` and paced as
    /// `pacing` asks. Output of the request's panes wakes it through
    /// `output`.
    pub(crate) fn spawn(
        name: String,
        renderer: MetalRenderer,
        request: MetalFrameRequest,
        surface: SurfaceState,
        min_frame_interval: Duration,
        pacing: Pacing,
        output: Arc<MetalOutputWake>,
    ) -> std::io::Result<Self> {
        let panes = request_panes(&request);
        let request = Arc::new(Mutex::new(Arc::new(request)));
        let driver_request = Arc::clone(&request);
        let presented_size = Arc::new(AtomicU64::new(0));
        let driver_presented_size = Arc::clone(&presented_size);
        #[cfg(test)]
        let rows_rebuilt = Arc::new(Mutex::new(std::collections::HashMap::new()));
        #[cfg(test)]
        let driver_rows_rebuilt = Arc::clone(&rows_rebuilt);
        let handoff = MetalRendererHandoff::new(renderer);
        let thread = RenderThread::spawn_paced(name, surface, min_frame_interval, move || {
            // The renderer is the last field: off macOS it is uninhabited,
            // and every field after it would never be evaluated.
            let mut driver = MetalDriver {
                request: driver_request,
                panes: MetalPanes::default(),
                fonts: None,
                presented_size: driver_presented_size,
                link_update: Rc::new(RefCell::new(None)),
                paced_by_link: pacing == Pacing::DisplayLink,
                opaque: None,
                #[cfg(test)]
                rows_rebuilt: driver_rows_rebuilt,
                renderer: Rc::new(handoff.into_renderer()),
            };
            // The link is created here, on the render thread whose run loop
            // runs it (ft-yccm0.4.1.3); before macOS 14 there is none.
            let link = if driver.paced_by_link {
                driver.renderer.display_link()
            } else {
                None
            };
            driver.paced_by_link = link.is_some();
            log::debug!(
                "render thread: paced by {}",
                if link.is_some() {
                    "the display link"
                } else {
                    "the max_fps interval"
                }
            );
            let source = link.map(|link| {
                Box::new(LinkSource {
                    link,
                    update: Rc::clone(&driver.link_update),
                    last_target: None,
                    periods: Vec::with_capacity(PERIOD_SAMPLES),
                }) as Box<dyn VsyncSource>
            });
            (driver, source)
        })?;
        output.set_panes(panes);
        output.set_waker(Some(thread.waker()));
        Ok(Self {
            thread,
            request,
            output,
            surface: std::cell::Cell::new(surface),
            #[cfg(test)]
            presented_size,
            #[cfg(test)]
            rows_rebuilt,
        })
    }

    /// Rows of `pane` rebuilt since the thread started.
    #[cfg(test)]
    pub(crate) fn rows_rebuilt(&self, pane: PaneId) -> u64 {
        self.rows_rebuilt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&pane)
            .copied()
            .unwrap_or(0)
    }

    /// The drawable size of the last presented frame; `(0, 0)` before one.
    #[cfg(test)]
    pub(crate) fn presented_size(&self) -> (u32, u32) {
        let size = self.presented_size.load(Ordering::Relaxed);
        ((size >> 32) as u32, size as u32)
    }

    /// Publishes the window state for the next frame and asks for it.
    pub(crate) fn publish(&self, request: MetalFrameRequest) {
        self.output.set_panes(request_panes(&request));
        *self.request.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(request);
        self.thread.waker().wake();
    }

    /// Posts the window's surface to the render thread when it changed.
    pub(crate) fn post_surface(&self, surface: SurfaceState) {
        if self.surface.replace(surface) != surface {
            self.thread.post_surface(surface);
        }
    }

    pub(crate) fn set_min_frame_interval(&self, interval: Duration) {
        self.thread.set_min_frame_interval(interval);
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> RenderThreadStats {
        self.thread.stats()
    }

    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.thread.is_running()
    }
}

impl Drop for MetalRenderThread {
    fn drop(&mut self) {
        // No output wakes for a thread that is going away; dropping
        // `thread` then stops and joins it.
        self.output.set_waker(None);
        self.output.set_panes(Vec::new());
    }
}

impl TermWindow {
    /// Moves `metal` to a render thread for this window. Gives it back when
    /// another owner shares it, so the caller can keep drawing on the main
    /// thread.
    pub(super) fn start_metal_render_thread(
        &mut self,
        metal: Rc<MetalRenderer>,
    ) -> Result<(), Rc<MetalRenderer>> {
        let renderer = Rc::try_unwrap(metal)?;
        let name = format!("ft-render-{}", self.mux_window_id);
        let request = self.metal_frame_request();
        match MetalRenderThread::spawn(
            name.clone(),
            renderer,
            request,
            self.metal_surface(),
            self.frame_interval(),
            Pacing::from_env(),
            Arc::clone(&self.metal_output_wake),
        ) {
            Ok(thread) => {
                log::info!("Metal frames are drawn on render thread {name}");
                self.metal_render = Some(thread);
            }
            // The thread never started, and its renderer went with it.
            Err(err) => log::error!("cannot start Metal render thread {name}: {err}"),
        }
        Ok(())
    }

    /// What the render thread's next frame needs from this window.
    pub(super) fn metal_frame_request(&mut self) -> MetalFrameRequest {
        let (mut panes, splits) = self.metal_window_panes();
        // After the panes: its hit-test items follow theirs, as in paint_pass.
        let chrome = self.metal_chrome_frame();
        let mut clear = self.metal_clear_color();
        if chrome
            .as_ref()
            .is_some_and(ChromeFrame::has_background_layers)
        {
            // Over background layers the WebGpu renderer fills neither the
            // window nor the panes' backgrounds; default cells are clear.
            clear = ClearColor::from_srgba(0.0, 0.0, 0.0, 0.0);
            for pane in &mut panes {
                pane.clear = clear;
            }
        }
        MetalFrameRequest {
            panes,
            splits,
            chrome,
            config: self.config.clone(),
            clear,
            grid: GridExtent::new(self.terminal_size.rows, self.terminal_size.cols),
            font_scale: self.fonts.get_font_scale(),
            opaque: self.window_background.is_empty()
                && self.config.window_background_opacity >= 1.0,
        }
    }

    /// The window's surface as the render thread sees it.
    pub(super) fn metal_surface(&self) -> SurfaceState {
        let pixels = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
        SurfaceState {
            pixel_width: pixels(self.dimensions.pixel_width),
            pixel_height: pixels(self.dimensions.pixel_height),
            dpi: pixels(self.dimensions.dpi),
            occluded: false,
            focused: self.focused.is_some(),
        }
    }

    /// Posts a changed size, DPI or focus to the render thread's mailbox.
    pub(super) fn post_metal_surface(&self) {
        if let Some(render) = &self.metal_render {
            render.post_surface(self.metal_surface());
        }
    }

    /// The main thread's whole share of a Metal frame while a render thread
    /// draws: publish what the frame needs and wake the thread. The damage up
    /// to here is the render thread's to draw.
    pub(super) fn publish_metal_frame(&mut self) -> bool {
        let captured_generation = self.damage_generation;
        self.begin_metal_font_read();
        let request = self.metal_frame_request();
        let Some(render) = &self.metal_render else {
            return false;
        };
        render.post_surface(self.metal_surface());
        render.publish(request);
        let settlement = super::apply_presented_render_attempt(
            &mut self.dirty_lines,
            self.damage_generation,
            &mut self.render_recovery_state,
            captured_generation,
        );
        metrics::counter!("gui.render.damage_settlement", "outcome" => settlement.label())
            .increment(1);
        self.render_wake_state.cancel();
        // The visual bell's fade needs new requests as it moves; the render
        // thread redraws blinking text and cursors itself (ft-yccm0.4.7.3).
        self.schedule_metal_animation(None);
        true
    }
}

/// The panes a request draws, whose output wakes the render thread: every
/// visible pane, and the pane an overlay reads.
fn request_panes(request: &MetalFrameRequest) -> Vec<PaneId> {
    let mut panes = Vec::with_capacity(request.panes.len());
    for pane in request
        .panes
        .iter()
        .filter_map(|pane| pane.inputs.pane.as_ref())
    {
        panes.push(pane.pane_id());
        let source = crate::selection::selection_source_pane(&**pane).pane_id();
        if source != pane.pane_id() {
            panes.push(source);
        }
    }
    panes
}

/// Native macOS tests: the real Metal renderer, unattached to a window
/// (`MetalRenderer::offscreen`), drawing a real local pane on a render
/// thread with fonts it loads itself.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::termwindow::metal_cells::MetalFrameInputs;
    use frankenterm_renderer_metal::PixelRect;
    use mux::pane::Pane;

    /// A local pane, `cat` on a pty, killed when dropped.
    struct TestPane(Arc<dyn Pane>);

    impl Drop for TestPane {
        fn drop(&mut self) {
            self.0.kill();
        }
    }

    impl TestPane {
        fn new(pane_id: PaneId) -> Self {
            let terminal = wezterm_term::Terminal::new(
                wezterm_term::TerminalSize {
                    rows: 24,
                    cols: 80,
                    dpi: 144,
                    pixel_width: 1280,
                    pixel_height: 768,
                },
                Arc::new(config::TermConfig::new()),
                "render-thread-test",
                "test",
                Box::new(Vec::<u8>::new()),
            );
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize::default())
                .unwrap();
            let writer = pair.master.take_writer().unwrap();
            let child = pair
                .slave
                .spawn_command(portable_pty::CommandBuilder::new("/bin/cat"))
                .unwrap();
            Self(Arc::new(mux::localpane::LocalPane::new(
                pane_id,
                terminal,
                child,
                pair.master,
                writer,
                pane_id,
                [0x41; 16],
                "render thread test".into(),
            )))
        }
    }

    /// `pane` drawn at `rect`: dimmed unless `active`.
    // Test rectangles are small pixel counts.
    #[allow(clippy::cast_precision_loss)]
    fn pane_request(pane: &TestPane, rect: PixelRect, active: bool) -> MetalPaneRequest {
        MetalPaneRequest {
            inputs: MetalFrameInputs {
                pane: Some(Arc::clone(&pane.0)),
                viewport_top: None,
                config: config::configuration(),
                focused: active,
                selection: None,
                hover: None,
                grid_origin: [rect.x as f32, rect.y as f32],
                compose: None,
                bell_cursor: None,
                fallback_ready: None,
                blink_pin: None,
                password_input: false,
            },
            rect,
            clear: ClearColor::from_srgba(0.0, 0.0, 0.0, 1.0),
            hsb: (!active).then_some([1.0, 0.8, 0.7]),
        }
    }

    fn window_request(panes: Vec<MetalPaneRequest>) -> MetalFrameRequest {
        MetalFrameRequest {
            panes,
            splits: Vec::new(),
            chrome: None,
            config: config::configuration(),
            clear: ClearColor::from_srgba(0.0, 0.0, 0.0, 1.0),
            grid: GridExtent::new(24, 80),
            font_scale: 1.0,
            opaque: true,
        }
    }

    fn request(pane: &TestPane) -> MetalFrameRequest {
        config::use_test_configuration();
        window_request(vec![pane_request(
            pane,
            PixelRect::new(0, 0, 1280, 768),
            true,
        )])
    }

    fn surface(pixel_width: u32, pixel_height: u32) -> SurfaceState {
        SurfaceState {
            pixel_width,
            pixel_height,
            dpi: 144,
            occluded: false,
            focused: true,
        }
    }

    /// The threading tests pace to the interval: an offscreen layer is on no
    /// display, so they do not depend on a display link ticking for it.
    fn spawn(pane: &TestPane, output: &Arc<MetalOutputWake>) -> MetalRenderThread {
        spawn_paced(pane, output, Pacing::Interval)
    }

    fn spawn_paced(
        pane: &TestPane,
        output: &Arc<MetalOutputWake>,
        pacing: Pacing,
    ) -> MetalRenderThread {
        MetalRenderThread::spawn(
            "ft-render-test".to_string(),
            MetalRenderer::offscreen().expect("this Mac runs the Metal front end"),
            request(pane),
            surface(1280, 768),
            Duration::from_millis(4),
            pacing,
            Arc::clone(output),
        )
        .unwrap()
    }

    /// Waits up to 20 s for `done`.
    fn eventually(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        done()
    }

    /// The acceptance test: while a thread named `main`, standing in for
    /// the AppKit thread, is blocked for 200 ms, output keeps reaching the
    /// pane's terminal, the delivering side wakes the render thread
    /// directly, and real Metal frames of the pane keep being presented.
    ///
    /// The output is applied as the mux's pty reader applies parsed output.
    /// Writing to the pane's pty instead blocked once `cat`'s echo filled the
    /// pty buffers, since no reader drains them here.
    #[test]
    fn frames_keep_presenting_while_the_main_thread_is_blocked() {
        let pane = TestPane::new(9_412_001);
        let pane_id = pane.0.pane_id();
        let output = Arc::new(MetalOutputWake::default());
        let render = spawn(&pane, &output);
        assert!(eventually(|| render.stats().frames_presented > 0));

        let before = render.stats();
        let feeding = std::sync::atomic::AtomicBool::new(true);
        let blocked_for = std::thread::scope(|scope| {
            let feeding = &feeding;
            let output = &output;
            let pane = &pane;
            scope.spawn(move || {
                use termwiz::escape::{Action, ControlCode};
                let mut line = 0;
                while feeding.load(Ordering::Acquire) {
                    pane.0
                        .perform_actions(vec![
                            Action::PrintString(format!("output line {line}")),
                            Action::Control(ControlCode::CarriageReturn),
                            Action::Control(ControlCode::LineFeed),
                        ])
                        .expect("the pane takes the output");
                    assert!(output.wake_for(pane_id), "the drawn pane wakes its thread");
                    line += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            let blocked_for = std::thread::Builder::new()
                .name("main".to_string())
                .spawn_scoped(scope, || {
                    let started = Instant::now();
                    std::thread::sleep(Duration::from_millis(200));
                    started.elapsed()
                })
                .unwrap()
                .join()
                .unwrap();
            feeding.store(false, Ordering::Release);
            blocked_for
        });
        let after = render.stats();
        let presented = after.frames_presented - before.frames_presented;
        eprintln!(
            "[BENCH] render thread: {presented} Metal frames presented while main was blocked for {blocked_for:?}"
        );
        // At most ~50 at the 4 ms interval; honest slack for a loaded host.
        assert!(
            presented >= 10,
            "{presented} frames presented while the main thread was blocked for {blocked_for:?}"
        );
        assert!(!output.wake_for(pane_id + 1), "other panes do not wake it");
    }

    /// A resize storm: hundreds of surface posts while output keeps waking
    /// the thread. The drawable follows at frame boundaries, posts between
    /// frames coalesce, and the window ends showing the last size.
    #[test]
    fn a_resize_storm_ends_presenting_at_the_last_size() {
        let pane = TestPane::new(9_412_002);
        let pane_id = pane.0.pane_id();
        let output = Arc::new(MetalOutputWake::default());
        let render = spawn(&pane, &output);
        assert!(eventually(|| render.stats().frames_presented > 0));
        const POSTS: u32 = 400;
        for n in 0..POSTS {
            render.post_surface(surface(800 + n, 500 + n / 2));
            output.wake_for(pane_id);
            if n % 20 == 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let last = (800 + POSTS - 1, 500 + (POSTS - 1) / 2);
        assert!(
            eventually(|| render.presented_size() == last),
            "the window shows {:?}, not the last size {:?}",
            render.presented_size(),
            last
        );
        let stats = render.stats();
        assert!(stats.reconfigures >= 2);
        assert!(
            stats.reconfigures < u64::from(POSTS),
            "{} reconfigures for {POSTS} posts",
            stats.reconfigures
        );
    }

    /// nv9: the throughput harness checks its FPS meter against the GUI's
    /// own FrameLedger, which read 0 presents/s from a Metal window. Every
    /// frame the render thread presents now counts there. Other tests may
    /// present at the same time, so the ledger's growth is a lower bound.
    #[test]
    fn every_presented_frame_counts_in_the_frame_ledger() {
        let pane = TestPane::new(9_412_005);
        let pane_id = pane.0.pane_id();
        let output = Arc::new(MetalOutputWake::default());
        let before = FrameLedger::global().snapshot().presented_total;
        let render = spawn(&pane, &output);
        assert!(eventually(|| {
            output.wake_for(pane_id);
            render.stats().frames_presented >= 5
        }));
        let presented = render.stats().frames_presented;
        let counted = FrameLedger::global().snapshot().presented_total - before;
        assert!(
            counted >= presented,
            "{presented} frames presented, {counted} counted in the frame ledger"
        );
    }

    /// Dropping the handle stops output wakes, then stops and joins the
    /// thread, which drops the renderer (waiting out its in-flight frames)
    /// on the render thread.
    #[test]
    fn dropping_the_handle_joins_the_render_thread() {
        let pane = TestPane::new(9_412_003);
        let pane_id = pane.0.pane_id();
        let output = Arc::new(MetalOutputWake::default());
        let render = spawn(&pane, &output);
        assert!(eventually(|| render.stats().frames_presented > 0));
        assert!(render.is_running());
        assert!(output.wake_for(pane_id));
        let started = Instant::now();
        drop(render);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "joining took {:?}",
            started.elapsed()
        );
        assert!(!output.wake_for(pane_id), "a stopped thread is not woken");
    }

    /// ft-yccm0.4.1.3 acceptance: paced by the real `CAMetalDisplayLink`,
    /// every frame is built on a link callback, into that callback's
    /// drawable, and an idle window pauses its link.
    ///
    /// Ignored: the link calls back only for a layer on a display, and the
    /// offscreen test layer is in no window. Its first native run
    /// (2026-10-07) saw 0 ticks in 1 s. It needs an on-screen window, which
    /// belongs in the dev-GUI end-to-end run, not in a unit test.
    #[test]
    #[ignore = "CAMetalDisplayLink needs a layer on a display; the offscreen test layer gets no callbacks"]
    fn frames_are_presented_only_from_display_link_callbacks() {
        use frankenterm_gui::render_thread::IDLE_TICKS_BEFORE_PAUSE;

        let pane = TestPane::new(9_412_004);
        let pane_id = pane.0.pane_id();
        let output = Arc::new(MetalOutputWake::default());
        let render = spawn_paced(&pane, &output, Pacing::DisplayLink);
        // Keep the pane dirty for a second, as streaming output does.
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(1) {
            output.wake_for(pane_id);
            std::thread::sleep(Duration::from_millis(2));
        }
        let busy = render.stats();
        eprintln!("[BENCH] display link: {busy:?} after 1 s of output");
        assert!(
            busy.vsync_ticks > 0,
            "the display link never ticked (is the test layer on a display?)"
        );
        assert!(busy.frames_presented > 0, "no frame was presented");
        assert!(
            busy.frames_built <= busy.vsync_ticks,
            "{} frames from {} link callbacks: a frame was built off a callback",
            busy.frames_built,
            busy.vsync_ticks
        );

        // Quiet: within a few ticks the link pauses and stops ticking.
        std::thread::sleep(Duration::from_millis(200));
        let paused = render.stats();
        std::thread::sleep(Duration::from_millis(300));
        let idle = render.stats();
        assert!(
            idle.vsync_ticks - paused.vsync_ticks <= u64::from(IDLE_TICKS_BEFORE_PAUSE),
            "an idle window kept its link ticking: {} ticks in 300 ms",
            idle.vsync_ticks - paused.vsync_ticks
        );
        assert_eq!(
            idle.frames_built, paused.frames_built,
            "an idle window drew"
        );
    }

    /// ft-yccm0.4.6 acceptance: two panes side by side, one active. Output
    /// in the inactive pane wakes the render thread from the delivering
    /// side, and the frames that follow rebuild that pane's changed row and
    /// none of the active pane's rows.
    #[test]
    fn output_in_an_inactive_pane_wakes_the_thread_and_redraws_only_its_rows() {
        let active = TestPane::new(9_446_001);
        let inactive = TestPane::new(9_446_002);
        let (active_id, inactive_id) = (active.0.pane_id(), inactive.0.pane_id());
        config::use_test_configuration();
        let output = Arc::new(MetalOutputWake::default());
        let render = MetalRenderThread::spawn(
            "ft-render-test".to_string(),
            MetalRenderer::offscreen().expect("this Mac runs the Metal front end"),
            window_request(vec![
                pane_request(&active, PixelRect::new(0, 0, 640, 768), true),
                pane_request(&inactive, PixelRect::new(640, 0, 640, 768), false),
            ]),
            surface(1280, 768),
            Duration::from_millis(4),
            Pacing::Interval,
            Arc::clone(&output),
        )
        .unwrap();
        assert!(output.wake_for(active_id), "the active pane wakes it");
        assert!(output.wake_for(inactive_id), "the inactive pane wakes it");
        assert!(
            !output.wake_for(9_446_003),
            "a pane it does not draw does not wake it"
        );
        // The first frame builds every row of both panes.
        assert!(eventually(|| {
            render.rows_rebuilt(active_id) > 0 && render.rows_rebuilt(inactive_id) > 0
        }));
        // Settled: woken frames are presented and rebuild no row.
        let counts = || {
            (
                render.rows_rebuilt(active_id),
                render.rows_rebuilt(inactive_id),
                render.stats().frames_presented,
            )
        };
        assert!(
            eventually(|| {
                let before = counts();
                output.wake_for(active_id);
                std::thread::sleep(Duration::from_millis(50));
                let after = counts();
                after.2 > before.2 && (after.0, after.1) == (before.0, before.1)
            }),
            "the panes never settled: {:?}",
            counts()
        );

        let (active_before, inactive_before, frames_before) = counts();
        inactive
            .0
            .perform_actions(vec![termwiz::escape::Action::PrintString(
                "output in the inactive pane".into(),
            )])
            .expect("the pane takes the output");
        // As the thread delivering the mux notification does.
        assert!(output.wake_for(inactive_id));
        assert!(
            eventually(|| render.rows_rebuilt(inactive_id) > inactive_before),
            "the inactive pane's changed row was never redrawn"
        );
        // A few more frames, each woken by the inactive pane.
        assert!(eventually(|| {
            output.wake_for(inactive_id);
            render.stats().frames_presented >= frames_before + 3
        }));
        let (active_after, inactive_after, frames_after) = counts();
        let rebuilt = inactive_after - inactive_before;
        eprintln!(
            "[BENCH] inactive-pane output: {} frames, {rebuilt} rows of the inactive pane and {} of the active pane rebuilt",
            frames_after - frames_before,
            active_after - active_before
        );
        assert_eq!(
            active_after, active_before,
            "the active pane rebuilt rows for output it never had"
        );
        assert!(
            rebuilt <= 2,
            "{rebuilt} rows rebuilt for one line of output"
        );
    }
}
