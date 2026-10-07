//! The Metal front end's render thread (ft-yccm0.4.1.2).
//!
//! With `front_end = "Metal"`, the window's renderer is attached on the main
//! thread, as AppKit requires, then moved to a render thread of its own
//! ([`frankenterm_gui::render_thread`]). From then on the render thread
//! captures the active pane's changed rows, rebuilds its scene and glyphs,
//! and encodes and presents every frame. The main thread only publishes what
//! a frame needs from the window ([`MetalFrameRequest`], when it would have
//! painted) and the window's surface (size, DPI, focus) through the render
//! thread's lock-free mailbox. Output of the pane being drawn wakes the
//! render thread straight from the thread that delivers the mux
//! notification ([`MetalOutputWake`]), with no main-thread task per batch.
//!
//! The render thread keeps its own fonts, built for the window's DPI and
//! font scale, since a `FontConfiguration` cannot leave the main thread.

use super::TermWindow;
use super::metal_cells::{MetalFrame, MetalFrameInputs};
use crate::utilsprites::RenderMetrics;
use frankenterm_font::FontConfiguration;
#[cfg(test)]
use frankenterm_gui::render_thread::RenderThreadStats;
use frankenterm_gui::render_thread::{
    FrameDriver, FrameReport, RenderThread, RenderWaker, Surface, SurfaceState,
};
use frankenterm_renderer_metal::{
    ClearColor, FrameOutcome, GridExtent, MetalRenderer, MetalRendererHandoff,
};
use mux::pane::PaneId;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// `FRANKENTERM_METAL_RENDER_THREAD=0` keeps Metal frames on the main thread.
pub(crate) const RENDER_THREAD_ENV: &str = "FRANKENTERM_METAL_RENDER_THREAD";

/// Whether Metal frames should be drawn on a render thread: the default,
/// unless [`RENDER_THREAD_ENV`] is `0`.
pub(crate) fn render_thread_enabled() -> bool {
    std::env::var_os(RENDER_THREAD_ENV).is_none_or(|value| value != "0")
}

/// What the main thread publishes for the render thread's next frame.
#[derive(Clone)]
pub(crate) struct MetalFrameRequest {
    pub(crate) inputs: MetalFrameInputs,
    pub(crate) clear: ClearColor,
    /// The grid a frame without a pane clears.
    pub(crate) grid: GridExtent,
    pub(crate) font_scale: f64,
}

const NO_PANE: usize = usize::MAX;

/// Wakes a window's render thread for the output of a pane it draws, on the
/// thread that delivers the mux notification (ft-yccm0.4.1.2).
#[derive(Debug)]
pub(crate) struct MetalOutputWake {
    waker: Mutex<Option<RenderWaker>>,
    /// The panes the render thread draws: the active pane and, under an
    /// overlay, the overlay pane. [`NO_PANE`] when unused.
    panes: [AtomicUsize; 2],
}

impl Default for MetalOutputWake {
    fn default() -> Self {
        Self {
            waker: Mutex::new(None),
            panes: [AtomicUsize::new(NO_PANE), AtomicUsize::new(NO_PANE)],
        }
    }
}

impl MetalOutputWake {
    /// Wakes the render thread when it draws `pane_id`. False when there is
    /// no render thread or it does not draw that pane, so the caller takes
    /// the ordinary main-thread path.
    pub(crate) fn wake_for(&self, pane_id: PaneId) -> bool {
        if !self
            .panes
            .iter()
            .any(|pane| pane.load(Ordering::Acquire) == pane_id)
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

    fn set_panes(&self, panes: [Option<PaneId>; 2]) {
        for (slot, pane) in self.panes.iter().zip(panes) {
            slot.store(pane.unwrap_or(NO_PANE), Ordering::Release);
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
    renderer: Rc<MetalRenderer>,
    request: Arc<Mutex<Arc<MetalFrameRequest>>>,
    frame: Option<MetalFrame>,
    fonts: Option<RenderFonts>,
    /// The drawable size of the last presented frame, width in the high 32
    /// bits: what the window shows now.
    presented_size: Arc<AtomicU64>,
}

impl MetalDriver {
    /// Fonts for `request` at `dpi`, rebuilt only when the configuration,
    /// DPI or font scale changed.
    fn fonts(&mut self, request: &MetalFrameRequest, dpi: u32) -> anyhow::Result<&RenderFonts> {
        let key = (
            request.inputs.config.generation(),
            dpi,
            request.font_scale.to_bits(),
        );
        if self.fonts.as_ref().is_none_or(|fonts| fonts.key != key) {
            let dpi_usize = usize::try_from(dpi)?;
            let fonts = Rc::new(FontConfiguration::new(
                Some(request.inputs.config.clone()),
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
        // explicit CATransaction); fonts follow the DPI in `frame`.
        let (width, height, dpi) = surface.state.geometry();
        log::debug!("render thread: drawable {width}x{height} at {dpi} dpi");
    }

    fn frame(&mut self, surface: &Surface) -> FrameReport {
        let request = Arc::clone(&self.request.lock().unwrap_or_else(PoisonError::into_inner));
        let (width, height, dpi) = surface.state.geometry();
        let (fonts, metrics) = match self.fonts(&request, dpi) {
            Ok(fonts) => (Rc::clone(&fonts.fonts), fonts.metrics.clone()),
            Err(err) => {
                log::error!("render thread: cannot load fonts: {err:#}");
                return FrameReport::default();
            }
        };
        let uniforms = MetalFrame::update(
            &mut self.frame,
            &request.inputs,
            &fonts,
            &metrics,
            &self.renderer,
        );
        let outcome = match (self.frame.as_ref(), uniforms.as_ref()) {
            (Some(frame), Some(uniforms)) => self.renderer.render_frame(
                width,
                height,
                &super::metal_frame_scene(frame, uniforms, request.clear),
            ),
            _ => self
                .renderer
                .render_clear(width, height, request.grid, request.clear),
        };
        match outcome {
            Ok(FrameOutcome::Presented) => {
                let (width, height) = self.renderer.drawable_size();
                self.presented_size.store(
                    (u64::from(width) << 32) | u64::from(height),
                    Ordering::Relaxed,
                );
                FrameReport {
                    presented: true,
                    redraw_at: None,
                }
            }
            Ok(FrameOutcome::ZeroSize) => FrameReport::default(),
            Err(err) => {
                // A drawable or frame slot that stayed busy past its wait:
                // try again at the next frame the interval allows.
                metrics::counter!("gui.metal.render_thread.frame_failed").increment(1);
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
}

impl MetalRenderThread {
    /// Moves `renderer` to a new render thread that draws `request` on
    /// `surface`, paced to `min_frame_interval`. Output of the request's
    /// panes wakes it through `output`.
    pub(crate) fn spawn(
        name: String,
        renderer: MetalRenderer,
        request: MetalFrameRequest,
        surface: SurfaceState,
        min_frame_interval: Duration,
        output: Arc<MetalOutputWake>,
    ) -> std::io::Result<Self> {
        let panes = request_panes(&request);
        let request = Arc::new(Mutex::new(Arc::new(request)));
        let driver_request = Arc::clone(&request);
        let presented_size = Arc::new(AtomicU64::new(0));
        let driver_presented_size = Arc::clone(&presented_size);
        let handoff = MetalRendererHandoff::new(renderer);
        // The renderer is the last field: off macOS it is uninhabited, and
        // every field after it would never be evaluated.
        let thread = RenderThread::spawn(name, surface, min_frame_interval, move || MetalDriver {
            request: driver_request,
            frame: None,
            fonts: None,
            presented_size: driver_presented_size,
            renderer: Rc::new(handoff.into_renderer()),
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
        })
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
        self.output.set_panes([None, None]);
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
        MetalFrameRequest {
            inputs: self.metal_frame_inputs(),
            clear: self.metal_clear_color(),
            grid: GridExtent::new(self.terminal_size.rows, self.terminal_size.cols),
            font_scale: self.fonts.get_font_scale(),
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
        true
    }
}

/// The panes a request draws, whose output wakes the render thread.
fn request_panes(request: &MetalFrameRequest) -> [Option<PaneId>; 2] {
    let pane = request.inputs.pane.as_ref();
    [
        pane.map(|pane| pane.pane_id()),
        pane.and_then(|pane| {
            let source = crate::selection::selection_source_pane(&**pane);
            let id = source.pane_id();
            (id != pane.pane_id()).then_some(id)
        }),
    ]
}

/// Native macOS tests: the real Metal renderer, unattached to a window
/// (`MetalRenderer::offscreen`), drawing a real local pane on a render
/// thread with fonts it loads itself.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use mux::pane::Pane;
    use std::io::Write as _;

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

    fn request(pane: &TestPane) -> MetalFrameRequest {
        config::use_test_configuration();
        MetalFrameRequest {
            inputs: MetalFrameInputs {
                pane: Some(Arc::clone(&pane.0)),
                viewport_top: None,
                config: config::configuration(),
                focused: true,
                selection: None,
                hover: None,
                grid_origin: [0.0, 0.0],
            },
            clear: ClearColor::from_srgba(0.0, 0.0, 0.0, 1.0),
            grid: GridExtent::new(24, 80),
            font_scale: 1.0,
        }
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

    fn spawn(pane: &TestPane, output: &Arc<MetalOutputWake>) -> MetalRenderThread {
        MetalRenderThread::spawn(
            "ft-render-test".to_string(),
            MetalRenderer::offscreen().expect("this Mac runs the Metal front end"),
            request(pane),
            surface(1280, 768),
            Duration::from_millis(4),
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
    /// pane through its pty, the delivering side wakes the render thread
    /// directly, and real Metal frames of the pane keep being presented.
    #[test]
    fn frames_keep_presenting_while_the_main_thread_is_blocked() {
        let pane = TestPane::new(9_412_001);
        let pane_id = pane.0.pane_id();
        let output = Arc::new(MetalOutputWake::default());
        let render = spawn(&pane, &output);
        assert!(eventually(|| render.stats().frames_presented > 0));

        let before = render.stats();
        let blocked_for = std::thread::scope(|scope| {
            let feeding = std::sync::atomic::AtomicBool::new(true);
            let feeding = &feeding;
            let output = &output;
            let pane = &pane;
            scope.spawn(move || {
                let mut line = 0;
                while feeding.load(Ordering::Acquire) {
                    writeln!(pane.0.writer(), "output line {line}").unwrap();
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
}
