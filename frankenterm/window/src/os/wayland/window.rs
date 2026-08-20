use std::any::Any;
use std::cell::RefCell;
use std::cmp::max;
use std::convert::TryFrom;
use std::io::{ErrorKind, Read};
use std::num::NonZeroU32;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use config::ConfigHandle;
use frankenterm_font::FontConfiguration;
use futures_util::future::{AbortHandle, AbortRegistration, Abortable};
use promise::{BrokenPromise, Future, Promise};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    WaylandWindowHandle, WindowHandle,
};
use smithay_client_toolkit::compositor::{CompositorHandler, SurfaceData, SurfaceDataExt};
use smithay_client_toolkit::data_device_manager::ReadPipe;
use smithay_client_toolkit::globals::GlobalData;
use smithay_client_toolkit::reexports::csd_frame::{
    DecorationsFrame, FrameAction, ResizeEdge, WindowState as SCTKWindowState,
};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge as XdgResizeEdge;
use smithay_client_toolkit::seat::pointer::CursorIcon;
use smithay_client_toolkit::shell::xdg::fallback_frame::FallbackFrame;
use smithay_client_toolkit::shell::xdg::window::{
    DecorationMode, Window as XdgWindow, WindowConfigure, WindowDecorations as Decorations,
    WindowHandler,
};
use smithay_client_toolkit::shell::xdg::XdgSurface;
use smithay_client_toolkit::shell::WaylandSurface;
use wayland_client::protocol::wl_callback::WlCallback;
use wayland_client::protocol::wl_keyboard::{Event as WlKeyboardEvent, KeyState};
use wayland_client::protocol::wl_pointer::{ButtonState, WlPointer};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection as WConnection, Dispatch, Proxy, QueueHandle};
use wayland_egl::{is_available as egl_is_available, WlEglSurface};
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;
use wezterm_input_types::{
    KeyboardLedStatus, Modifiers, MouseButtons, MouseEvent, MouseEventKind, MousePress,
    ScreenPoint, WindowDecorations,
};

use crate::wayland::WaylandConnection;
use crate::x11::{KeyboardWithFallback, WaylandRepeatSeed};
use crate::{
    Appearance, Clipboard, Connection, ConnectionOps, Dimensions, MouseCursor, Point, Rect,
    RequestedWindowGeometry, ResizeIncrement, ResolvedGeometry, Window, WindowEvent,
    WindowEventSender, WindowKeyEvent, WindowOps, WindowState,
};

/// Wayland-specific coordinate conversion methods for Dimensions
trait WaylandDimensions {
    fn dpi_factor(&self) -> f64;
    fn pixels_to_surface(&self, pixels: i32) -> i32;
    fn surface_to_pixels(&self, surface: i32) -> i32;
}

impl WaylandDimensions for Dimensions {
    fn dpi_factor(&self) -> f64 {
        self.dpi as f64 / crate::DEFAULT_DPI
    }

    fn pixels_to_surface(&self, pixels: i32) -> i32 {
        // Take care to round up, otherwise we can lose a pixel
        // and that can effectively lose the final row of the terminal
        (pixels as f64 / self.dpi_factor()).ceil() as i32
    }

    fn surface_to_pixels(&self, surface: i32) -> i32 {
        (surface as f64 * self.dpi_factor()).ceil() as i32
    }
}

fn checked_surface_dimensions(width: u32, height: u32) -> Option<(i32, i32)> {
    let dimensions = (i32::try_from(width).ok()?, i32::try_from(height).ok()?);
    (dimensions.0 > 0 && dimensions.1 > 0).then_some(dimensions)
}

fn checked_pixel_dimensions(width: usize, height: usize) -> Option<(i32, i32)> {
    let dimensions = (i32::try_from(width).ok()?, i32::try_from(height).ok()?);
    (dimensions.0 > 0 && dimensions.1 > 0).then_some(dimensions)
}

fn validate_resize_increments(incr: ResizeIncrement) -> anyhow::Result<()> {
    if incr.x == 0 || incr.y == 0 {
        bail!(
            "Wayland resize increments must be non-zero, got {}x{}",
            incr.x,
            incr.y
        );
    }
    Ok(())
}

use super::copy_and_paste::{set_pipe_nonblocking, CopyAndPaste};
use super::pointer::{PendingMouse, PointerUserData};
use super::state::WaylandState;

static INVALID_KEY_REPEAT_INFO_REPORTED: AtomicBool = AtomicBool::new(false);
static INVALID_WAYLAND_KEYCODE_REPORTED: AtomicBool = AtomicBool::new(false);

fn key_repeat_timing(rate: i32, delay_ms: i32) -> Option<(Duration, Duration)> {
    if rate <= 0 || delay_ms < 0 {
        return None;
    }

    let rate = u64::try_from(rate).ok()?;
    let delay_ms = u64::try_from(delay_ms).ok()?;
    let gap_ms = 1_000_u64.div_ceil(rate);
    Some((
        Duration::from_millis(delay_ms),
        Duration::from_millis(gap_ms),
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyRepeatTimerPlan {
    Wait(Duration),
    Dispatch {
        repeat_count: u16,
        next_due: Duration,
    },
}

fn key_repeat_timer_plan(
    elapsed: Duration,
    next_due: Duration,
    gap: Duration,
) -> Option<KeyRepeatTimerPlan> {
    let gap_nanos = gap.as_nanos();
    if gap_nanos == 0 {
        return None;
    }

    let Some(overdue) = elapsed.checked_sub(next_due) else {
        return Some(KeyRepeatTimerPlan::Wait(next_due - elapsed));
    };
    let overdue_nanos = overdue.as_nanos();
    let due = overdue_nanos.checked_div(gap_nanos)?.saturating_add(1);
    let repeat_count = u16::try_from(due).unwrap_or(u16::MAX);
    let remainder_nanos = overdue_nanos % gap_nanos;
    let until_next_nanos = gap_nanos.checked_sub(remainder_nanos)?;
    let until_next = Duration::from_nanos(u64::try_from(until_next_nanos).ok()?);
    let next_due = elapsed.checked_add(until_next)?;
    Some(KeyRepeatTimerPlan::Dispatch {
        repeat_count,
        next_due,
    })
}

fn key_repeat_first_due(
    elapsed: Duration,
    delay: Duration,
    gap: Duration,
    last_dispatch: Option<Duration>,
) -> Option<Duration> {
    if gap.is_zero() {
        return None;
    }
    match last_dispatch {
        Some(last_dispatch) => last_dispatch.checked_add(gap).map(|due| due.max(elapsed)),
        None if elapsed >= delay => Some(elapsed),
        None => Some(delay),
    }
}

fn report_invalid_key_repeat_info(rate: i32, delay_ms: i32) {
    if INVALID_KEY_REPEAT_INFO_REPORTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        log::warn!(
            "Disabling Wayland key repeat after invalid repeat_info: rate={rate}, delay={delay_ms}"
        );
    }
}

fn report_invalid_wayland_keycode(key: u32) {
    if INVALID_WAYLAND_KEYCODE_REPORTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        log::warn!("Ignoring Wayland keyboard input with overflowing keycode {key}");
    }
}

struct KeyRepeatAbort {
    handle: AbortHandle,
}

impl KeyRepeatAbort {
    fn new_pair() -> (Self, AbortRegistration) {
        let (handle, registration) = AbortHandle::new_pair();
        (Self { handle }, registration)
    }
}

impl Drop for KeyRepeatAbort {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct KeyRepeatTimerLease {
    identity: Arc<()>,
    _abort: KeyRepeatAbort,
}

struct HeldKeyRepeat {
    seed: WaylandRepeatSeed,
    origin: Instant,
    last_dispatch: Option<Duration>,
    timer: Option<KeyRepeatTimerLease>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompositorRepeatTransition {
    RetainHeld,
    RetireMismatched,
    Untracked,
}

fn compositor_repeat_transition(
    held_key: Option<u32>,
    repeated_key: u32,
) -> CompositorRepeatTransition {
    match held_key {
        Some(held_key) if held_key == repeated_key => CompositorRepeatTransition::RetainHeld,
        Some(_) => CompositorRepeatTransition::RetireMismatched,
        None => CompositorRepeatTransition::Untracked,
    }
}

fn key_repeat_window_event(mut event: WindowKeyEvent, repeat_count: u16) -> WindowEvent {
    match &mut event {
        WindowKeyEvent::KeyEvent(key) => {
            key.repeat_count = repeat_count;
            if let Some(raw) = key.raw.as_mut() {
                raw.repeat_count = repeat_count;
            }
        }
        WindowKeyEvent::RawKeyEvent(raw) => {
            raw.repeat_count = repeat_count;
        }
    }
    match event {
        WindowKeyEvent::KeyEvent(key) => WindowEvent::KeyEvent(key),
        WindowKeyEvent::RawKeyEvent(raw) => WindowEvent::RawKeyEvent(raw),
    }
}

async fn run_key_repeat(
    window_id: usize,
    seed: WaylandRepeatSeed,
    identity: Arc<()>,
    origin: Instant,
    initial_due: Duration,
    gap: Duration,
) {
    let mut next_due = initial_due;
    loop {
        let Some(plan) = key_repeat_timer_plan(origin.elapsed(), next_due, gap) else {
            log::warn!("Stopping Wayland key repeat for window {window_id}: invalid timer state");
            return;
        };

        let repeat_count = match plan {
            KeyRepeatTimerPlan::Wait(wait) => {
                promise::spawn::sleep(wait).await;
                continue;
            }
            KeyRepeatTimerPlan::Dispatch {
                repeat_count,
                next_due: following_due,
            } => {
                next_due = following_due;
                repeat_count
            }
        };

        {
            let (handle, translation) = {
                let Some(conn) = WaylandConnection::get() else {
                    log::debug!(
                        "Stopping Wayland key repeat for window {window_id}: connection unavailable"
                    );
                    return;
                };
                let conn = conn.wayland();
                let Some(handle) = conn.window_by_id(window_id) else {
                    return;
                };
                let translation = {
                    let state = conn.wayland_state.borrow();
                    let Some(mapper) = state.keyboard_mapper.as_ref() else {
                        log::debug!(
                            "Stopping Wayland key repeat for window {window_id}: keyboard mapper unavailable"
                        );
                        return;
                    };
                    let Some(event) = mapper.translate_wayland_repeat(&seed) else {
                        report_invalid_wayland_keycode(seed.key());
                        return;
                    };
                    event
                };
                (handle, translation)
            };

            let mut inner = handle.borrow_mut();
            let is_current = inner.key_repeat.as_ref().is_some_and(|held| {
                held.timer
                    .as_ref()
                    .is_some_and(|timer| Arc::ptr_eq(&timer.identity, &identity))
            });
            if !is_current || inner.window.is_none() {
                return;
            }
            if let Some(held) = inner.key_repeat.as_mut() {
                held.last_dispatch = Some(origin.elapsed());
            }
            inner
                .events
                .dispatch(key_repeat_window_event(translation, repeat_count));
        }

        // A slow event handler can leave the absolute deadline overdue. Yield
        // after each coalesced dispatch so close, release, focus, and settings
        // changes can run and abort this lease before another repeat batch.
        futures_lite::future::yield_now().await;
    }
}

enum WaylandWindowEvent {
    Close,
    Request(WindowConfigure),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct WaylandWindow(usize);

struct PendingFirstConfigure {
    promise: Option<Promise<()>>,
}

impl PendingFirstConfigure {
    fn new() -> (Self, Future<()>) {
        let mut promise = Promise::new();
        let future = promise
            .get_future()
            .expect("pending first-configure promise should create a future");
        (
            Self {
                promise: Some(promise),
            },
            future,
        )
    }

    fn resolve(&mut self) {
        if let Some(mut promise) = self.promise.take() {
            promise.ok(());
        }
    }
}

impl Drop for PendingFirstConfigure {
    fn drop(&mut self) {
        if let Some(mut promise) = self.promise.take() {
            promise.err(BrokenPromise {}.into());
        }
    }
}

fn new_pending_first_configure() -> (PendingFirstConfigure, Future<()>) {
    PendingFirstConfigure::new()
}

fn resolve_pending_first_configure(pending_first_configure: &mut Option<PendingFirstConfigure>) {
    if let Some(mut notify) = pending_first_configure.take() {
        notify.resolve();
    }
}

impl WaylandWindow {
    pub async fn new_window<F>(
        class_name: &str,
        name: &str,
        geometry: RequestedWindowGeometry,
        config: Option<&ConfigHandle>,
        _font_config: Rc<FontConfiguration>,
        event_handler: F,
    ) -> anyhow::Result<Window>
    where
        F: 'static + FnMut(WindowEvent, &Window),
    {
        let config = match config {
            Some(c) => c.clone(),
            None => config::configuration(),
        };

        let conn = WaylandConnection::get()
            .ok_or_else(|| {
                anyhow!(
                    "new_window must be called on the gui thread after Connection:init has succeed",
                )
            })?
            .wayland();

        let window_id = conn.next_window_id()?;
        let pending_event = Arc::new(Mutex::new(PendingEvent::default()));

        let (pending_first_configure, wait_configure) = new_pending_first_configure();

        let qh = conn.event_queue.borrow().handle();

        // We need user data so we can get the window_id => WaylandWindowInner during a handler
        let surface_data = SurfaceUserData {
            surface_data: SurfaceData::default(),
            window_id,
        };
        let surface = {
            let compositor = &conn.wayland_state.borrow().compositor;
            compositor.create_surface_with_data(&qh, surface_data)
        };

        let ResolvedGeometry {
            x: _,
            y: _,
            width,
            height,
        } = conn.resolve_geometry(geometry);

        let dimensions = Dimensions {
            pixel_width: width,
            pixel_height: height,
            dpi: config.dpi.unwrap_or(crate::DEFAULT_DPI) as usize,
        };

        let window = {
            let xdg_shell = &conn.wayland_state.borrow().xdg;
            xdg_shell.create_window(surface.clone(), Decorations::RequestServer, &qh)
        };

        window.set_app_id(class_name.to_string());
        window.set_title(name.to_string());
        let decorations = config.window_decorations;

        let decor_mode = if decorations == WindowDecorations::NONE {
            None
        } else if decorations == WindowDecorations::default() {
            Some(DecorationMode::Server)
        } else {
            Some(DecorationMode::Client)
        };
        window.request_decoration_mode(decor_mode);

        let mut window_frame = {
            let wayland_state = &conn.wayland_state.borrow();
            let shm = &wayland_state.shm;
            let subcompositor = wayland_state.subcompositor.clone();
            FallbackFrame::new(&window, shm, subcompositor, qh.clone())
                .expect("failed to create csd frame")
        };
        let hidden = !matches!(decor_mode, Some(DecorationMode::Client));
        window_frame.set_hidden(hidden);
        if !hidden {
            window_frame.resize(
                u32::try_from(dimensions.pixel_width)
                    .ok()
                    .and_then(NonZeroU32::new)
                    .ok_or_else(|| anyhow!("dimensions {dimensions:?} are invalid"))?,
                u32::try_from(dimensions.pixel_height)
                    .ok()
                    .and_then(NonZeroU32::new)
                    .ok_or_else(|| anyhow!("dimensions {dimensions:?} are invalid"))?,
            );
        }

        window.set_min_size(Some((32, 32)));
        let (x, y) = window_frame.location();
        let surface_width = dimensions.pixels_to_surface(dimensions.pixel_width as i32);
        let surface_height = dimensions.pixels_to_surface(dimensions.pixel_height as i32);
        window
            .xdg_surface()
            .set_window_geometry(x, y, surface_width, surface_height);
        window.commit();

        let copy_and_paste = CopyAndPaste::create();
        let pending_mouse = PendingMouse::create(window_id, &copy_and_paste);

        conn.wayland_state
            .borrow_mut()
            .surface_to_pending
            .insert(surface.id(), Arc::clone(&pending_mouse));

        let appearance = conn.get_appearance();

        let inner = Rc::new(RefCell::new(WaylandWindowInner {
            events: WindowEventSender::new(event_handler),
            surface_factor: 1.0,
            copy_and_paste,
            invalidated: false,
            window: Some(window),
            window_frame,
            dimensions,
            resize_increments: None,
            window_state: WindowState::default(),
            last_mouse_coords: Point::new(0, 0),
            mouse_buttons: MouseButtons::NONE,
            hscroll_remainder: 0.0,
            vscroll_remainder: 0.0,

            modifiers: Modifiers::NONE,
            leds: KeyboardLedStatus::empty(),

            key_repeat: None,
            pending_event,
            pending_mouse,

            pending_first_configure: Some(pending_first_configure),
            frame_callback: None,
            frame_callback_chain_depth: 0,
            frame_callback_chain_depth_peak: 0,

            text_cursor: None,
            appearance,

            config,
            active_output_names: Vec::new(),

            title: None,

            wegl_surface: None,
            gl_state: None,
        }));

        let window_handle = Window::Wayland(WaylandWindow(window_id));

        inner
            .borrow_mut()
            .events
            .assign_window(window_handle.clone());

        inner.borrow().update_window_background_blur();

        {
            let windows = &conn.wayland_state.borrow().windows;
            windows.borrow_mut().insert(window_id, inner.clone());
        };

        wait_configure.await?;

        Ok(window_handle)
    }
}

#[async_trait(?Send)]
impl WindowOps for WaylandWindow {
    fn show(&self) {
        WaylandConnection::with_window_inner(self.0, |inner| {
            inner.show();
            Ok(())
        });
    }

    fn notify<T: Any + Send + Sync>(&self, t: T)
    where
        Self: Sized,
    {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            inner
                .events
                .dispatch(WindowEvent::Notification(Box::new(t)));
            Ok(())
        });
    }

    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>> {
        let window = self.0;
        let reservation = crate::reserve_window_main_thread(
            promise::spawn::MainThreadServiceClass::Interactive,
            64 * 1024,
            "Wayland enable OpenGL",
        )
        .map_err(|rejected| anyhow::anyhow!("Wayland OpenGL admission rejected: {rejected:?}"))?;
        reservation.spawn_local(async move {
            let Some(conn) = Connection::get() else {
                bail!("cannot enable OpenGL: Wayland connection unavailable");
            };
            if let Some(handle) = conn.wayland().window_by_id(window) {
                let mut inner = handle.borrow_mut();
                inner.enable_opengl()
            } else {
                anyhow::bail!("invalid window");
            }
        }).into_task().await
    }

    fn hide(&self) {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            let Some(window) = inner.window_or_log("hide request") else {
                return Ok(());
            };
            window.set_minimized();
            Ok(())
        });
    }

    fn close(&self) {
        let window_id = self.0;
        match crate::reserve_window_main_thread(
            promise::spawn::MainThreadServiceClass::Topology,
            4 * 1024,
            "Wayland window close",
        ) {
            Ok(reservation) => reservation
                .spawn(async move {
                    let Some(connection) = WaylandConnection::get() else {
                        log::debug!(
                            "Dropping Wayland close for window {window_id}: connection unavailable"
                        );
                        return;
                    };
                    let connection = connection.wayland();
                    let Some(handle) = connection.window_by_id(window_id) else {
                        return;
                    };
                    let surface_id = handle
                        .borrow()
                        .window
                        .as_ref()
                        .map(|window| window.wl_surface().id());

                    {
                        let mut state = connection.wayland_state.borrow_mut();
                        let authority_cleanup = state
                            .clear_destroyed_window_authorities(window_id, surface_id.as_ref());
                        if authority_cleanup.keyboard {
                            if let (Some(text_input), Some(keyboard)) =
                                (&state.text_input, &state.keyboard)
                            {
                                if let Some(input) = text_input.get_text_input_for_keyboard(keyboard)
                                {
                                    input.disable();
                                    input.commit();
                                }
                            }
                        }
                        if let Some(surface_id) = surface_id.as_ref() {
                            if let Some(text_input) = &state.text_input {
                                text_input.forget_surface_id(surface_id);
                            }
                            state.surface_to_pending.remove(surface_id);
                        }
                    }

                    // Publish destroyed input authority before dispatching Destroyed,
                    // but keep the registry entry alive during the callback so
                    // re-entrant window queries observe a coherent closing object.
                    handle.borrow_mut().close();

                    let removed = connection
                        .wayland_state
                        .borrow_mut()
                        .windows
                        .get_mut()
                        .remove(&window_id);
                    debug_assert!(
                        removed
                            .as_ref()
                            .is_some_and(|removed| Rc::ptr_eq(removed, &handle)),
                        "closed Wayland window registry entry changed before removal"
                    );
                })
                .detach(),
            Err(rejected) => log::error!(
                "Wayland close for window {window_id} was rejected by the main-thread scheduler: {rejected:?}"
            ),
        }
    }

    fn set_cursor(&self, cursor: Option<MouseCursor>) {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            inner.set_cursor(cursor);
            Ok(())
        });
    }

    fn invalidate(&self) {
        WaylandConnection::with_window_inner(self.0, |inner| {
            inner.invalidate();
            Ok(())
        });
    }

    fn set_text_cursor_position(&self, cursor: Rect) {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            inner.set_text_cursor_position(cursor);
            Ok(())
        });
    }

    fn set_title(&self, title: &str) {
        let title = title.to_owned();
        WaylandConnection::with_window_inner(self.0, |inner| {
            inner.set_title(title);
            Ok(())
        });
    }

    fn set_inner_size(&self, width: usize, height: usize) {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            inner.set_inner_size(width, height);
            Ok(())
        });
    }

    fn set_resize_increments(&self, incr: ResizeIncrement) {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            if let Err(err) = inner.set_resize_increments(incr) {
                log::error!("Wayland set_resize_increments failed: {err:#}");
            }
            Ok(())
        });
    }

    fn get_clipboard(&self, clipboard: Clipboard) -> Future<String> {
        let mut promise = Promise::new();
        let Some(future) = promise.get_future() else {
            return Future::err(anyhow!(
                "new Wayland clipboard promise did not yield a future"
            ));
        };
        let promise = Arc::new(Mutex::new(promise));
        let watcher_reservation = match crate::reserve_window_main_thread(
            promise::spawn::MainThreadServiceClass::Interactive,
            4 * 1024,
            "Wayland clipboard completion",
        ) {
            Ok(reservation) => reservation,
            Err(rejected) => {
                lock_or_recover(
                    &promise,
                    "recording Wayland clipboard scheduler admission failure",
                )
                .err(anyhow!(
                    "Wayland clipboard completion admission rejected: {rejected:?}"
                ));
                return future;
            }
        };
        // Clone for the setup closure so the original `promise` stays owned here
        // and remains usable on the error path below (line `promise_on_error`).
        let promise_for_setup = Arc::clone(&promise);
        let window_future = WaylandConnection::with_window_inner(self.0, move |inner| {
            let read = match inner.copy_and_paste.lock() {
                Ok(mut copy_and_paste) => copy_and_paste.get_clipboard_data(clipboard)?,
                Err(_) => bail!("Wayland copy-and-paste lock was poisoned while reading clipboard"),
            };
            let promise_for_thread = Arc::clone(&promise_for_setup);
            let spawn_result = std::thread::Builder::new()
                .name("wayland-clipboard-read".to_string())
                .spawn(move || {
                    let mut promise =
                        lock_or_recover(&promise_for_thread, "resolving clipboard read promise");
                    match read_pipe_with_timeout(read) {
                        Ok(result) => {
                            // Normalize the text to unix line endings, otherwise
                            // copying from eg: firefox inserts a lot of blank
                            // lines, and that is super annoying.
                            promise.ok(result.replace("\r\n", "\n"));
                        }
                        Err(e) => {
                            log::error!("while reading clipboard: {}", e);
                            promise.err(anyhow!("{}", e));
                        }
                    };
                });
            if let Err(err) = spawn_result {
                let mut promise = lock_or_recover(
                    &promise_for_setup,
                    "recording clipboard reader thread spawn failure",
                );
                promise.err(anyhow!(
                    "unable to spawn Wayland clipboard reader thread: {err}"
                ));
            }
            Ok(())
        });
        let promise_on_error = Arc::clone(&promise);
        watcher_reservation
            .spawn_local(async move {
                if let Err(err) = window_future.await {
                    let mut promise = lock_or_recover(
                        &promise_on_error,
                        "recording Wayland clipboard read setup failure",
                    );
                    promise.err(err);
                }
            })
            .detach();
        future
    }

    fn set_clipboard(&self, clipboard: Clipboard, text: String) {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            match inner.copy_and_paste.lock() {
                Ok(mut copy_and_paste) => copy_and_paste.set_clipboard_data(clipboard, text),
                Err(_) => {
                    log::error!("Wayland copy-and-paste lock was poisoned while setting clipboard");
                }
            }
            Ok(())
        });
    }

    fn toggle_fullscreen(&self) {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            let Some(window) = inner.window_or_log("fullscreen toggle") else {
                return Ok(());
            };
            if inner.window_state.contains(WindowState::FULL_SCREEN) {
                window.unset_fullscreen();
            } else {
                window.set_fullscreen(None);
            }
            Ok(())
        });
    }

    fn maximize(&self) {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            inner.maximize();
            Ok(())
        });
    }

    fn restore(&self) {
        WaylandConnection::with_window_inner(self.0, move |inner| {
            inner.restore();
            Ok(())
        });
    }

    fn config_did_change(&self, config: &ConfigHandle) {
        let config = config.clone();
        WaylandConnection::with_window_inner(self.0, move |inner| {
            inner.config_did_change(config);
            Ok(())
        });
    }
}
#[derive(Default, Clone, Debug)]
pub(crate) struct PendingEvent {
    pub(crate) close: bool,
    pub(crate) had_configure_event: bool,
    refresh_decorations: bool,
    /// Synthetic dimension-only configure events (queued from
    /// `set_inner_size`) and live compositor-driven `WindowConfigure`
    /// events (which carry full WM state) are tracked separately.
    /// The two cannot trivially merge: a synthetic configure can
    /// arrive while a window_configure is pending — for example
    /// during a maximize→minimize toggle — and `dispatch_pending_event`
    /// applies them in a specific order (window_configure WM-state
    /// first, then dimensions). Combining the fields would require
    /// reordering that dispatch path. Audited under ft-mpc9b.3.2:
    /// this is independent of the frame-callback batching that
    /// bead targets and is left as-is. See ft-c9arc / Wayland live-
    /// resize work for any future merge.
    pub(crate) configure: Option<(u32, u32)>,
    pub(crate) window_configure: Option<WindowConfigure>,
    pub(crate) dpi: Option<i32>,
    pub(crate) window_state: Option<WindowState>,
}

fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, context: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::error!("Wayland mutex was poisoned while {context}; recovering inner state");
            poisoned.into_inner()
        }
    }
}

pub(crate) fn read_pipe_with_timeout(mut file: ReadPipe) -> anyhow::Result<String> {
    let mut result = Vec::new();

    set_pipe_nonblocking(file.as_raw_fd())?;

    let mut pfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    let mut buf = [0u8; 8192];

    loop {
        pfd.revents = 0;
        let poll_result = unsafe { libc::poll(&mut pfd, 1, 3000) };
        if poll_result > 0 {
            if pfd.revents & libc::POLLNVAL != 0 {
                bail!("read pipe became invalid: revents={:#x}", pfd.revents);
            }
            if pfd.revents & libc::POLLIN == 0 {
                if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                    break;
                }
                continue;
            }

            match file.read(&mut buf) {
                Ok(0) => {
                    break;
                }
                Ok(size) => {
                    result.extend_from_slice(&buf[..size]);
                }
                Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {
                    continue;
                }
                Err(e) => bail!("error reading from pipe: {e}"),
            }
        } else if poll_result == 0 {
            bail!("timed out reading from pipe");
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            bail!("error polling read pipe: {err}");
        }
    }

    Ok(String::from_utf8(result)?)
}

pub struct WaylandWindowInner {
    pub(crate) events: WindowEventSender,
    surface_factor: f64,
    copy_and_paste: Arc<Mutex<CopyAndPaste>>,
    window: Option<XdgWindow>,
    pub(super) window_frame: FallbackFrame<WaylandState>,
    dimensions: Dimensions,
    resize_increments: Option<ResizeIncrement>,
    window_state: WindowState,
    last_mouse_coords: Point,
    mouse_buttons: MouseButtons,
    hscroll_remainder: f64,
    vscroll_remainder: f64,
    modifiers: Modifiers,
    leds: KeyboardLedStatus,
    key_repeat: Option<HeldKeyRepeat>,
    pub(super) pending_event: Arc<Mutex<PendingEvent>>,
    pub(super) pending_mouse: Arc<Mutex<PendingMouse>>,
    pending_first_configure: Option<PendingFirstConfigure>,
    frame_callback: Option<WlCallback>,
    /// Number of frame callbacks currently in flight with the
    /// compositor. Should never exceed 1 given the structural early
    /// returns at `invalidate` (~1070), `do_paint` (~1153), and the
    /// take in `next_frame_is_ready` (~1192) — see ft-mpc9b.3.2.
    /// Tracked so a Linux integration test can assert the invariant
    /// under the resize-storm reproducer the bead targets.
    frame_callback_chain_depth: u32,
    /// Peak chain depth observed since window construction. Surfaced
    /// for the visual-regression harness (RQ-S* SLOs) and `ft doctor`.
    frame_callback_chain_depth_peak: u32,
    invalidated: bool,
    // font_config: Rc<FontConfiguration>,
    text_cursor: Option<Rect>,
    appearance: Appearance,
    config: ConfigHandle,
    active_output_names: Vec<String>,
    // cache the title for comparison to avoid spamming
    // the compositor with updates that don't actually change it
    title: Option<String>,
    // wegl_surface is listed before gl_state because it
    // must be dropped before gl_state otherwise the underlying
    // libraries will segfault on shutdown
    wegl_surface: Option<WlEglSurface>,
    gl_state: Option<Rc<glium::backend::Context>>,
}

impl WaylandWindowInner {
    pub(super) fn cancel_key_repeat(&mut self) {
        self.key_repeat.take();
    }

    fn replace_key_repeat(
        &mut self,
        seed: WaylandRepeatSeed,
        window_id: usize,
        rate: i32,
        delay_ms: i32,
    ) {
        self.cancel_key_repeat();
        self.key_repeat = Some(HeldKeyRepeat {
            seed,
            origin: Instant::now(),
            last_dispatch: None,
            timer: None,
        });
        self.refresh_key_repeat_timing(window_id, rate, delay_ms);
    }

    fn refresh_key_repeat_timing(&mut self, window_id: usize, rate: i32, delay_ms: i32) {
        let Some(held) = self.key_repeat.as_mut() else {
            return;
        };
        held.timer.take();
        let Some((delay, gap)) = key_repeat_timing(rate, delay_ms) else {
            if rate < 0 || delay_ms < 0 {
                report_invalid_key_repeat_info(rate, delay_ms);
            }
            return;
        };

        let origin = held.origin;
        let Some(initial_due) =
            key_repeat_first_due(origin.elapsed(), delay, gap, held.last_dispatch)
        else {
            log::warn!("Stopping Wayland key repeat for window {window_id}: invalid first due");
            return;
        };
        let seed = held.seed.clone();
        let identity = Arc::new(());
        let task_identity = Arc::clone(&identity);
        let (abort, registration) = KeyRepeatAbort::new_pair();
        held.timer = Some(KeyRepeatTimerLease {
            identity,
            _abort: abort,
        });
        match crate::reserve_window_main_thread(
            promise::spawn::MainThreadServiceClass::Input,
            4 * 1024,
            "Wayland key repeat",
        ) {
            Ok(reservation) => reservation
                .spawn(async move {
                    let _ = Abortable::new(
                        run_key_repeat(window_id, seed, task_identity, origin, initial_due, gap),
                        registration,
                    )
                    .await;
                })
                .detach(),
            Err(rejected) => {
                held.timer.take();
                log::error!(
                    "Wayland key repeat for window {window_id} was rejected by the main-thread scheduler: {rejected:?}"
                );
            }
        }
    }

    fn close(&mut self) {
        self.cancel_key_repeat();
        self.events.dispatch(WindowEvent::Destroyed);
        self.window.take();
    }

    fn show(&mut self) {
        log::trace!("WaylandWindowInner show: {:?}", self.window);
        if self.window.is_none() {
            return;
        }

        // If the do_paint function has been called previously, calling it again will not
        // send the NeedRepaint event. This results in the window not being displayed
        // correctly.
        // Therefore, when frame_callback is set to some, we need to send the NeedRepaint
        // event again to ensure the window is displayed.
        // Fix: https://github.com/wezterm/wezterm/issues/5103
        if self.frame_callback.is_some() {
            self.events.dispatch(WindowEvent::NeedRepaint);
        }

        self.request_paint("show");
    }

    fn refresh_frame(&mut self) {
        if self.window_frame.is_dirty() && !self.window_frame.is_hidden() {
            self.window_frame.draw();
        }
    }

    fn enable_opengl(&mut self) -> anyhow::Result<Rc<glium::backend::Context>> {
        let Some(connection) = Connection::get() else {
            bail!("cannot enable OpenGL: Wayland connection unavailable");
        };
        let wayland_conn = connection.wayland();
        let mut wegl_surface = None;

        log::trace!("Enable opengl");

        let gl_state = if !egl_is_available() {
            Err(anyhow!("!egl_is_available"))
        } else {
            let window = self
                .window
                .as_ref()
                .ok_or(anyhow!("Window does not exist"))?;
            let object_id = window.wl_surface().id();
            let (pixel_width, pixel_height) =
                checked_pixel_dimensions(self.dimensions.pixel_width, self.dimensions.pixel_height)
                    .ok_or_else(|| {
                        anyhow!(
                    "Wayland EGL dimensions {}x{} must fit the positive i32 coordinate range",
                    self.dimensions.pixel_width,
                    self.dimensions.pixel_height
                )
                    })?;

            let surface = WlEglSurface::new(object_id, pixel_width, pixel_height)?;

            log::trace!("WEGL Surface here {:?}", surface);

            let gl_state = match wayland_conn.gl_connection.borrow().as_ref() {
                Some(glconn) => {
                    crate::egl::GlState::create_wayland_with_existing_connection(glconn, &surface)
                }
                None => crate::egl::GlState::create_wayland(
                    Some(wayland_conn.connection.backend().display_ptr() as *const _),
                    &surface,
                ),
            };
            wegl_surface = Some(surface);
            gl_state
        };
        let gl_state = gl_state.map(Rc::new).and_then(|state| unsafe {
            wayland_conn
                .gl_connection
                .borrow_mut()
                .replace(Rc::clone(state.get_connection()));
            Ok(glium::backend::Context::new(
                Rc::clone(&state),
                true,
                if cfg!(debug_assertions) {
                    glium::debug::DebugCallbackBehavior::DebugMessageOnError
                } else {
                    glium::debug::DebugCallbackBehavior::Ignore
                },
            )?)
        })?;

        self.gl_state.replace(gl_state.clone());
        self.wegl_surface = wegl_surface;

        Ok(gl_state)
    }

    fn get_dpi_factor(&self) -> f64 {
        self.dimensions.dpi_factor()
    }

    fn surface_to_pixels(&self, surface: i32) -> i32 {
        self.dimensions.surface_to_pixels(surface)
    }

    fn pixels_to_surface(&self, pixels: i32) -> i32 {
        self.dimensions.pixels_to_surface(pixels)
    }

    pub(super) fn dispatch_dropped_files(&mut self, paths: Vec<PathBuf>) {
        self.events.dispatch(WindowEvent::DroppedFile(paths));
    }

    pub(crate) fn dispatch_pending_mouse(&mut self) {
        let pending_mouse = Arc::clone(&self.pending_mouse);

        if let Some((x, y)) = PendingMouse::coords(&pending_mouse) {
            let coords = Point::new(
                self.surface_to_pixels(x as i32) as isize,
                self.surface_to_pixels(y as i32) as isize,
            );
            self.last_mouse_coords = coords;
            let event = MouseEvent {
                kind: MouseEventKind::Move,
                coords,
                screen_coords: ScreenPoint::new(
                    coords.x + self.dimensions.pixel_width as isize,
                    coords.y + self.dimensions.pixel_height as isize,
                ),
                mouse_buttons: self.mouse_buttons,
                modifiers: self.modifiers,
            };
            self.events.dispatch(WindowEvent::MouseEvent(event));
            self.refresh_frame();
        }

        while let Some((button, state)) = PendingMouse::next_button(&pending_mouse) {
            let button_mask = match button {
                MousePress::Left => MouseButtons::LEFT,
                MousePress::Right => MouseButtons::RIGHT,
                MousePress::Middle => MouseButtons::MIDDLE,
            };

            if state == ButtonState::Pressed {
                self.mouse_buttons |= button_mask;
            } else {
                self.mouse_buttons -= button_mask;
            }

            let event = MouseEvent {
                kind: match state {
                    ButtonState::Pressed => MouseEventKind::Press(button),
                    ButtonState::Released => MouseEventKind::Release(button),
                    _ => continue,
                },
                coords: self.last_mouse_coords,
                screen_coords: ScreenPoint::new(
                    self.last_mouse_coords.x + self.dimensions.pixel_width as isize,
                    self.last_mouse_coords.y + self.dimensions.pixel_height as isize,
                ),
                mouse_buttons: self.mouse_buttons,
                modifiers: self.modifiers,
            };
            self.events.dispatch(WindowEvent::MouseEvent(event));
        }

        if let Some((value_x, value_y)) = PendingMouse::scroll(&pending_mouse) {
            let factor = self.get_dpi_factor();

            if value_x.signum() != self.hscroll_remainder.signum() {
                // reset accumulator when changing scroll direction
                self.hscroll_remainder = 0.0;
            }
            let scaled_x = (value_x * factor) + self.hscroll_remainder;
            let discrete_x = scaled_x.trunc();
            self.hscroll_remainder = scaled_x - discrete_x;
            if discrete_x != 0. {
                let event = MouseEvent {
                    kind: MouseEventKind::HorzWheel(-discrete_x as i16),
                    coords: self.last_mouse_coords,
                    screen_coords: ScreenPoint::new(
                        self.last_mouse_coords.x + self.dimensions.pixel_width as isize,
                        self.last_mouse_coords.y + self.dimensions.pixel_height as isize,
                    ),
                    mouse_buttons: self.mouse_buttons,
                    modifiers: self.modifiers,
                };
                self.events.dispatch(WindowEvent::MouseEvent(event));
            }

            if value_y.signum() != self.vscroll_remainder.signum() {
                self.vscroll_remainder = 0.0;
            }
            let scaled_y = (value_y * factor) + self.vscroll_remainder;
            let discrete_y = scaled_y.trunc();
            self.vscroll_remainder = scaled_y - discrete_y;
            if discrete_y != 0. {
                let event = MouseEvent {
                    kind: MouseEventKind::VertWheel(-discrete_y as i16),
                    coords: self.last_mouse_coords,
                    screen_coords: ScreenPoint::new(
                        self.last_mouse_coords.x + self.dimensions.pixel_width as isize,
                        self.last_mouse_coords.y + self.dimensions.pixel_height as isize,
                    ),
                    mouse_buttons: self.mouse_buttons,
                    modifiers: self.modifiers,
                };
                self.events.dispatch(WindowEvent::MouseEvent(event));
            }
        }

        if !PendingMouse::in_window(&pending_mouse) {
            self.events.dispatch(WindowEvent::MouseLeave);
            self.refresh_frame();
        }
    }

    pub(crate) fn dispatch_pending_event(&mut self) {
        let mut pending;
        {
            let mut pending_events =
                lock_or_recover(&self.pending_event, "dispatching pending window events");
            pending = pending_events.clone();
            *pending_events = PendingEvent::default();
        }

        if pending.close {
            self.events.dispatch(WindowEvent::CloseRequested);
        }

        if let Some(window_state) = pending.window_state.take() {
            log::debug!(
                "dispatch_pending_event self.window_state={:?}, pending:{:?}",
                self.window_state,
                window_state
            );
            self.window_state = window_state;
        }

        if pending.configure.is_none() && pending.dpi.is_some() {
            // Synthesize a pending configure event for the dpi change
            let converted =
                checked_pixel_dimensions(self.dimensions.pixel_width, self.dimensions.pixel_height)
                    .and_then(|(width, height)| {
                        Some((
                            u32::try_from(self.pixels_to_surface(width)).ok()?,
                            u32::try_from(self.pixels_to_surface(height)).ok()?,
                        ))
                    });
            if let Some(dimensions) = converted {
                pending.configure.replace(dimensions);
            } else {
                log::error!(
                    "Cannot synthesize Wayland DPI configure for invalid dimensions {:?}",
                    self.dimensions
                );
            }
            log::debug!("synthesize configure with {:?}", pending.configure);
        }

        if let Some(ref window_config) = pending.window_configure {
            self.window_frame.update_state(window_config.state);
            self.window_frame
                .update_wm_capabilities(window_config.capabilities);
        }

        if let Some((mut w, mut h)) = pending.configure.take() {
            log::trace!("Pending configure: w:{w}, h{h} -- {:?}", self.window);
            'valid_configure: {
                if self.window.is_none() {
                    break 'valid_configure;
                }
                let Some((surface_width, surface_height)) = checked_surface_dimensions(w, h) else {
                    log::error!(
                        "Ignoring invalid Wayland configure dimensions {w}x{h}: values must fit the positive i32 coordinate range"
                    );
                    break 'valid_configure;
                };
                let surface_udata = SurfaceUserData::from_wl(self.surface());
                let factor = surface_udata.surface_data.scale_factor() as f64;
                let old_dimensions = self.dimensions;

                let dpi = self
                    .active_output_names
                    .last()
                    .map(String::as_str)
                    .map(|name| {
                        super::output::effective_wayland_dpi(
                            name,
                            factor,
                            self.config.dpi,
                            &self.config.dpi_by_screen,
                        )
                    })
                    .unwrap_or_else(|| self.config.dpi.unwrap_or(factor * crate::DEFAULT_DPI))
                    as usize;

                // Do this early because this affects surface_to_pixels/pixels_to_surface
                self.dimensions.dpi = dpi;

                let mut pixel_width = self.surface_to_pixels(surface_width);
                let mut pixel_height = self.surface_to_pixels(surface_height);

                if self.window_state.can_resize() {
                    self.window_frame.set_resizable(true);
                    if let Some(incr) = self.resize_increments {
                        let min_width = i32::from(incr.base_width) + i32::from(incr.x);
                        let min_height = i32::from(incr.base_height) + i32::from(incr.y);
                        let extra_width = (pixel_width - incr.base_width as i32) % incr.x as i32;
                        let extra_height = (pixel_height - incr.base_height as i32) % incr.y as i32;
                        let desired_pixel_width = max(pixel_width - extra_width, min_width);
                        let desired_pixel_height = max(pixel_height - extra_height, min_height);
                        let adjusted_surface_width = self.pixels_to_surface(desired_pixel_width);
                        let adjusted_surface_height = self.pixels_to_surface(desired_pixel_height);
                        let (Ok(adjusted_w), Ok(adjusted_h)) = (
                            u32::try_from(adjusted_surface_width),
                            u32::try_from(adjusted_surface_height),
                        ) else {
                            log::error!(
                                "Ignoring invalid Wayland resize-increment result {adjusted_surface_width}x{adjusted_surface_height}"
                            );
                            break 'valid_configure;
                        };
                        w = adjusted_w;
                        h = adjusted_h;
                        pixel_width = self.surface_to_pixels(adjusted_surface_width);
                        pixel_height = self.surface_to_pixels(adjusted_surface_height);
                    }
                }

                log::trace!("Resizing frame");
                if !self.window_frame.is_hidden() {
                    // Clamp the size to at least one surface heigh/width.
                    let width = NonZeroU32::new(w).unwrap_or(NonZeroU32::MIN);
                    let height = NonZeroU32::new(h).unwrap_or(NonZeroU32::MIN);
                    self.window_frame.resize(width, height);
                    pending.refresh_decorations = true
                }
                let (x, y) = self.window_frame.location();
                let surface_width = self.pixels_to_surface(pixel_width);
                let surface_height = self.pixels_to_surface(pixel_height);
                let Some(window) = self.window.as_mut() else {
                    break 'valid_configure;
                };
                window
                    .xdg_surface()
                    .set_window_geometry(x, y, surface_width, surface_height);
                // Compute the new pixel dimensions
                let (Ok(pixel_width_usize), Ok(pixel_height_usize)) =
                    (usize::try_from(pixel_width), usize::try_from(pixel_height))
                else {
                    log::error!(
                        "Ignoring invalid Wayland pixel dimensions {pixel_width}x{pixel_height}"
                    );
                    break 'valid_configure;
                };
                let new_dimensions = Dimensions {
                    pixel_width: pixel_width_usize,
                    pixel_height: pixel_height_usize,
                    dpi,
                };

                // Only trigger a resize if the new dimensions are different;
                // this makes things more efficient and a little more smooth
                if new_dimensions != old_dimensions {
                    self.dimensions = new_dimensions;

                    self.events.dispatch(WindowEvent::Resized {
                        dimensions: self.dimensions,
                        window_state: self.window_state,
                        // We don't know if we're live resizing or not, so
                        // assume no.
                        live_resizing: false,
                    });
                    // Avoid blurring by matching the scaling factor of the
                    // compositor; if it is going to double the size then
                    // we render at double the size anyway and tell it that
                    // the buffer is already doubled.
                    // Take care to detach the current buffer (managed by EGL),
                    // so that the compositor doesn't get annoyed by it not
                    // having dimensions that match the scale.
                    // The wegl_surface.resize won't take effect until
                    // we paint later on.
                    // We do this only if the scale has actually changed,
                    // otherwise interactive window resize will keep removing
                    // the window contents!
                    if let Some(wegl_surface) = self.wegl_surface.as_mut() {
                        wegl_surface.resize(pixel_width, pixel_height, 0, 0);
                    }
                    if self.surface_factor != factor {
                        if let Some(connection) = Connection::get() {
                            let wayland_conn = connection.wayland();
                            let wayland_state = wayland_conn.wayland_state.borrow();
                            let mut pool = wayland_state.mem_pool.borrow_mut();

                            // Make a "fake" buffer with the right dimensions, as
                            // simply detaching the buffer can cause wlroots-derived
                            // compositors consider the window to be unconfigured.
                            if let Ok((buffer, _bytes)) = pool.create_buffer(
                                factor as i32,
                                factor as i32,
                                (factor * 4.0) as i32,
                                wayland_client::protocol::wl_shm::Format::Argb8888,
                            ) {
                                self.surface().attach(Some(buffer.wl_buffer()), 0, 0);
                                self.surface().set_buffer_scale(factor as i32);
                                self.surface_factor = factor;
                            }
                        } else {
                            log::debug!(
                                "Skipping Wayland scale buffer update: connection unavailable"
                            );
                        }
                    }
                }
                self.request_paint("configure");
            }
        }
        if pending.refresh_decorations && self.window.is_some() {
            self.refresh_frame();
        }
        if pending.had_configure_event && self.window.is_some() {
            log::debug!("Had configured an event");
            // Allow window creation to complete.
            resolve_pending_first_configure(&mut self.pending_first_configure);
        }
    }

    fn set_cursor(&mut self, cursor: Option<MouseCursor>) {
        if !PendingMouse::in_window(&self.pending_mouse) {
            return;
        }

        let Some(conn) = Connection::get() else {
            log::debug!("Dropping Wayland cursor update: connection unavailable");
            return;
        };
        let conn = conn.wayland();
        let state = conn.wayland_state.borrow_mut();
        let pointer = match &state.pointer {
            Some(pointer) => pointer,
            None => return,
        };

        match cursor {
            Some(cursor) => {
                if let Err(err) = pointer.set_cursor(
                    &conn.connection,
                    match cursor {
                        MouseCursor::Arrow => CursorIcon::Default,
                        MouseCursor::Hand => CursorIcon::Pointer,
                        MouseCursor::SizeUpDown => CursorIcon::NsResize,
                        MouseCursor::SizeLeftRight => CursorIcon::EwResize,
                        MouseCursor::Text => CursorIcon::Text,
                    },
                ) {
                    log::error!("set_cursor: {}", err);
                }
            }
            None => {
                if let Err(err) = pointer.hide_cursor() {
                    log::error!("hide_cursor: {}", err)
                }
            }
        }
    }

    fn invalidate(&mut self) {
        if self.frame_callback.is_some() {
            self.invalidated = true;
            return;
        }
        self.request_paint("invalidate");
    }

    fn set_text_cursor_position(&mut self, rect: Rect) {
        let Some(conn) = WaylandConnection::get() else {
            log::debug!("Dropping Wayland text cursor update: connection unavailable");
            return;
        };
        let conn = conn.wayland();
        let state = conn.wayland_state.borrow();
        let surface = self.surface().clone();
        let keyboard_surface_id = state.keyboard_active_surface_id.borrow();
        let surface_id = surface.id();

        if keyboard_surface_id.as_ref() == Some(&surface_id)
            && self.text_cursor.map(|prior| prior != rect).unwrap_or(true)
        {
            self.text_cursor.replace(rect);

            let surface_udata = SurfaceUserData::from_wl(&surface);
            let factor = surface_udata.surface_data().scale_factor();

            if let Some(text_input) = &state.text_input {
                if let Some(input) = text_input.get_text_input_for_surface(&surface) {
                    input.set_cursor_rectangle(
                        rect.min_x() as i32 / factor,
                        rect.min_y() as i32 / factor,
                        rect.width() as i32 / factor,
                        rect.height() as i32 / factor,
                    );
                    input.commit();
                }
            }
        }
    }

    fn set_title(&mut self, title: String) {
        if let Some(last_title) = self.title.as_ref() {
            if last_title == &title {
                return;
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.set_title(title.clone());
        }
        self.refresh_frame();
        self.title = Some(title);
    }

    fn set_resize_increments(&mut self, incr: ResizeIncrement) -> anyhow::Result<()> {
        validate_resize_increments(incr)?;
        self.resize_increments.replace(incr);
        Ok(())
    }

    fn set_inner_size(&mut self, width: usize, height: usize) {
        let Some((pixel_width, pixel_height)) = checked_pixel_dimensions(width, height) else {
            log::error!(
                "Ignoring invalid Wayland inner size {width}x{height}: values must fit the positive i32 coordinate range"
            );
            self.events.dispatch(WindowEvent::SetInnerSizeCompleted);
            return;
        };
        let surface_width = self.pixels_to_surface(pixel_width);
        let surface_height = self.pixels_to_surface(pixel_height);
        let (Ok(surface_width), Ok(surface_height)) =
            (u32::try_from(surface_width), u32::try_from(surface_height))
        else {
            log::error!(
                "Ignoring invalid converted Wayland inner size {surface_width}x{surface_height}"
            );
            self.events.dispatch(WindowEvent::SetInnerSizeCompleted);
            return;
        };
        // window.resize() doesn't generate a configure event,
        // so we're going to fake one up, otherwise the window
        // contents don't reflect the real size until eg:
        // the focus is changed.
        {
            let mut pending_event =
                lock_or_recover(&self.pending_event, "queueing synthetic configure event");
            pending_event
                .configure
                .replace((surface_width, surface_height));
        }
        // apply the synthetic configure event to the inner surfaces
        self.dispatch_pending_event();

        self.events.dispatch(WindowEvent::SetInnerSizeCompleted);
    }

    fn do_paint(&mut self) -> anyhow::Result<()> {
        if self.window.is_none() {
            // We're likely in the middle of closing/destroying
            // the window; we've nothing to do here.
            return Ok(());
        }

        if self.frame_callback.is_some() {
            // Painting now won't be productive, so skip it but
            // remember that we need to be painted so that when
            // the compositor is ready for us, we can paint then.
            self.invalidated = true;
            return Ok(());
        }

        // Ask the compositor to wake us up when its time to paint the next frame,
        // note that this only happens _after_ the next commit
        let Some(conn) = WaylandConnection::get() else {
            self.invalidated = true;
            return Err(anyhow!("Wayland connection unavailable while painting"));
        };
        let conn = conn.wayland();
        let qh = conn.event_queue.borrow().handle();

        self.invalidated = false;

        let callback = self.surface().frame(&qh, self.surface().clone());

        log::trace!("do_paint - callback: {:?}", callback);
        let prior = self.frame_callback.replace(callback);
        // The structural guard at the top of this function should
        // make the prior callback always None here. Track the
        // chain depth so a Linux integration test can pin the
        // invariant against the resize-storm reproducer
        // (ft-mpc9b.3.2). See `frame_callback_chain_depth` field
        // doc on `WaylandWindowInner`.
        debug_assert!(
            prior.is_none(),
            "frame_callback_chain_depth invariant violated: \
             do_paint reached the frame() request with a callback already in flight"
        );
        self.frame_callback_chain_depth = self.frame_callback_chain_depth.saturating_add(1);
        if self.frame_callback_chain_depth > self.frame_callback_chain_depth_peak {
            self.frame_callback_chain_depth_peak = self.frame_callback_chain_depth;
        }
        if self.frame_callback_chain_depth > 1 {
            log::warn!(
                "wayland frame_callback chain depth = {} (peak {}); \
                 expected ≤ 1 — see ft-mpc9b.3.2",
                self.frame_callback_chain_depth,
                self.frame_callback_chain_depth_peak,
            );
        }

        // The repaint has the side of effect of committing the surface,
        // which is necessary for the frame callback to get triggered.
        // Ordering the repaint after requesting the callback ensures that
        // we will get woken at the appropriate time.
        // <https://github.com/wezterm/wezterm/issues/3468>
        // <https://github.com/wezterm/wezterm/issues/3126>
        self.events.dispatch(WindowEvent::NeedRepaint);

        Ok(())
    }

    fn request_paint(&mut self, context: &str) {
        if let Err(err) = self.do_paint() {
            log::debug!("Dropping Wayland repaint during {context}: {err:#}");
        }
    }

    fn window_or_log(&self, action: &str) -> Option<&XdgWindow> {
        match self.window.as_ref() {
            Some(window) => Some(window),
            None => {
                log::debug!("Dropping Wayland {action}: window unavailable");
                None
            }
        }
    }

    fn surface(&self) -> &WlSurface {
        self.window
            .as_ref()
            .expect("Window should exist")
            .wl_surface()
    }

    pub(crate) fn next_frame_is_ready(&mut self) {
        let prior = self.frame_callback.take();
        if prior.is_some() {
            // Decrement the chain-depth counter — pairs with the
            // increment after `surface().frame()` in `do_paint`.
            self.frame_callback_chain_depth = self.frame_callback_chain_depth.saturating_sub(1);
        }
        if self.invalidated {
            self.request_paint("frame callback");
        }
    }

    pub(crate) fn emit_focus(&mut self, mapper: Option<&mut KeyboardWithFallback>, focused: bool) {
        self.cancel_key_repeat();

        if focused {
            self.events.dispatch(WindowEvent::FocusChanged(true));
            // A Modifiers event may arrive while no surface has keyboard
            // focus. Preserve that compositor-authoritative XKB state and
            // publish it to the newly focused window instead of zeroing it.
            if let Some(mapper) = mapper {
                let modifiers = mapper.get_key_modifiers();
                let leds = mapper.get_led_status();
                if modifiers != self.modifiers || leds != self.leds {
                    self.modifiers = modifiers;
                    self.leds = leds;
                    self.events
                        .dispatch(WindowEvent::AdviseModifiersLedStatus(modifiers, leds));
                }
            }
        } else {
            // Clear state on focus loss so a chord that caused a focus change
            // cannot remain logically held in the unfocused window. Future
            // unfocused Modifiers events repopulate the mapper before Enter.
            self.modifiers = Modifiers::NONE;
            self.leds = KeyboardLedStatus::empty();
            if let Some(mapper) = mapper {
                mapper.update_modifier_state(0, 0, 0, 0);
            }
            self.events.dispatch(WindowEvent::FocusChanged(false));
        }
        self.text_cursor.take();
    }

    /// Retire this window during a direct keyboard-focus transfer while
    /// preserving the seat-global mapper for the destination window.
    pub(super) fn emit_focus_transfer_out(&mut self) {
        self.emit_focus(None, false);
    }

    pub(crate) fn appearance_changed(&mut self, appearance: Appearance) {
        if appearance != self.appearance {
            self.appearance = appearance;
            self.events
                .dispatch(WindowEvent::AppearanceChanged(appearance));
        }
    }

    pub(super) fn keyboard_event(
        &mut self,
        mapper: Option<&mut KeyboardWithFallback>,
        event: WlKeyboardEvent,
        window_id: usize,
        key_repeat_rate: i32,
        key_repeat_delay: i32,
    ) {
        match &event {
            WlKeyboardEvent::Enter { keys, .. } => {
                let key_codes = keys
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| u32::from_ne_bytes(*chunk))
                    .collect::<Vec<_>>();
                log::trace!("keyboard event: Enter with keys: {:?}", key_codes);
                self.emit_focus(mapper, true);
                return;
            }
            WlKeyboardEvent::Leave { .. } => {
                self.emit_focus(mapper, false);
                return;
            }
            _ => {}
        }

        let Some(mapper) = mapper else {
            log::debug!(
                "Ignoring Wayland keyboard event before keymap initialization: {:?}",
                event
            );
            return;
        };
        match event {
            WlKeyboardEvent::Key { key, state, .. } => {
                if key.checked_add(8).is_none() {
                    report_invalid_wayland_keycode(key);
                    self.cancel_key_repeat();
                    return;
                }
                let (pressed, compositor_repeated) = match state.into_result() {
                    Ok(KeyState::Pressed) => (true, false),
                    Ok(KeyState::Released) => (false, false),
                    Ok(KeyState::Repeated) => (true, true),
                    Ok(key_state) => {
                        log::warn!(
                            "Ignoring Wayland keyboard event with unsupported key state {key_state:?}"
                        );
                        self.cancel_key_repeat();
                        return;
                    }
                    Err(raw_state) => {
                        log::warn!(
                            "Ignoring Wayland keyboard event with unknown key state {raw_state:?}"
                        );
                        self.cancel_key_repeat();
                        return;
                    }
                };
                if compositor_repeated {
                    // Protocol v10 permits compositor-owned repeats while
                    // repeat_info advertises rate zero.  Abort any defensive
                    // local timer but retain the held-key origin: a later
                    // positive repeat_info hands repetition back to the
                    // client while this key is still down.
                    match compositor_repeat_transition(
                        self.key_repeat.as_ref().map(|held| held.seed.key()),
                        key,
                    ) {
                        CompositorRepeatTransition::RetainHeld => {
                            let held = self
                                .key_repeat
                                .as_mut()
                                .expect("matched held repeat must still exist");
                            held.timer.take();
                            held.last_dispatch = Some(held.origin.elapsed());
                        }
                        CompositorRepeatTransition::RetireMismatched
                        | CompositorRepeatTransition::Untracked => {
                            self.cancel_key_repeat();
                            let now = Instant::now();
                            self.key_repeat = Some(HeldKeyRepeat {
                                seed: WaylandRepeatSeed::Uncomposed { key },
                                origin: now,
                                last_dispatch: Some(Duration::ZERO),
                                timer: None,
                            });
                        }
                    }
                    let seed = self
                        .key_repeat
                        .as_ref()
                        .expect("compositor repeat must establish held metadata")
                        .seed
                        .clone();
                    if let Some(event) = mapper.translate_wayland_repeat(&seed) {
                        self.events.dispatch(key_repeat_window_event(event, 1));
                    } else {
                        report_invalid_wayland_keycode(key);
                        self.cancel_key_repeat();
                    }
                } else if let Some(seed) =
                    mapper.process_wayland_key(key, pressed, &mut self.events)
                {
                    self.replace_key_repeat(seed, window_id, key_repeat_rate, key_repeat_delay);
                } else if let Some(active) = self.key_repeat.as_ref() {
                    // important to check that it's the same key, because the release of the previously
                    // repeated key can come right after the press of the newly held key
                    if active.seed.key() == key {
                        self.cancel_key_repeat();
                    }
                }
            }
            WlKeyboardEvent::RepeatInfo { .. } => {
                self.refresh_key_repeat_timing(window_id, key_repeat_rate, key_repeat_delay);
            }
            WlKeyboardEvent::Modifiers { .. } => {
                let mods = mapper.get_key_modifiers();
                let leds = mapper.get_led_status();

                let changed = (mods != self.modifiers) || (leds != self.leds);

                self.modifiers = mapper.get_key_modifiers();
                self.leds = mapper.get_led_status();

                if changed {
                    self.events
                        .dispatch(WindowEvent::AdviseModifiersLedStatus(mods, leds));
                }
            }
            _ => {}
        }
    }

    pub(super) fn frame_action(&mut self, pointer: &WlPointer, serial: u32, action: FrameAction) {
        let Some(pointer_data) = pointer.data::<PointerUserData>() else {
            log::warn!("Ignoring Wayland frame action without pointer user data");
            return;
        };
        let seat = pointer_data.pdata.seat();
        match action {
            FrameAction::Close => self.events.dispatch(WindowEvent::CloseRequested),
            FrameAction::Minimize => {
                if let Some(window) = self.window_or_log("frame minimize action") {
                    window.set_minimized();
                }
            }
            FrameAction::Maximize => {
                if let Some(window) = self.window_or_log("frame maximize action") {
                    window.set_maximized();
                }
            }
            FrameAction::UnMaximize => {
                if let Some(window) = self.window_or_log("frame unmaximize action") {
                    window.unset_maximized();
                }
            }
            FrameAction::ShowMenu(x, y) => {
                if let Some(window) = self.window_or_log("frame menu action") {
                    window.show_window_menu(seat, serial, (x, y));
                }
            }
            FrameAction::Resize(edge) => {
                let edge = match edge {
                    ResizeEdge::None => XdgResizeEdge::None,
                    ResizeEdge::Top => XdgResizeEdge::Top,
                    ResizeEdge::Bottom => XdgResizeEdge::Bottom,
                    ResizeEdge::Left => XdgResizeEdge::Left,
                    ResizeEdge::TopLeft => XdgResizeEdge::TopLeft,
                    ResizeEdge::BottomLeft => XdgResizeEdge::BottomLeft,
                    ResizeEdge::Right => XdgResizeEdge::Right,
                    ResizeEdge::TopRight => XdgResizeEdge::TopRight,
                    ResizeEdge::BottomRight => XdgResizeEdge::BottomRight,
                    _ => return, // Realistically, there probably won't be any new edges added.
                };
                if let Some(window) = self.window_or_log("frame resize action") {
                    window.resize(seat, serial, edge);
                }
            }
            FrameAction::Move => {
                if let Some(window) = self.window_or_log("frame move action") {
                    window.move_(seat, serial);
                }
            }
            _ => log::warn!("unhandled FrameAction: {:?}", action),
        }
    }

    fn maximize(&mut self) {
        if let Some(window) = self.window.as_mut() {
            window.set_maximized();
        }
    }

    fn restore(&mut self) {
        if let Some(window) = self.window.as_mut() {
            window.unset_maximized();
        }
    }

    fn config_did_change(&mut self, config: ConfigHandle) {
        self.config = config;
        self.update_window_background_blur();
    }

    fn update_window_background_blur(&self) {
        let Some(conn) = WaylandConnection::get() else {
            log::debug!("Dropping Wayland background blur update: connection unavailable");
            return;
        };
        let conn = conn.wayland();
        let qh = conn.event_queue.borrow().handle();
        let wayland_state = conn.wayland_state.borrow();
        if let Some(manager) = &wayland_state.kde_blur_manager {
            let kde_blur = manager.create(self.surface(), &qh, GlobalData);
            if self.config.kde_window_background_blur {
                kde_blur.set_region(None);
            } else {
                kde_blur.release();
            }
            kde_blur.commit();
        }
    }
}

impl WaylandState {
    pub(super) fn window_by_id(&self, window_id: usize) -> Option<Rc<RefCell<WaylandWindowInner>>> {
        self.windows.borrow().get(&window_id).map(Rc::clone)
    }

    fn handle_window_event(&self, window: &XdgWindow, event: WaylandWindowEvent) {
        let surface_data = SurfaceUserData::from_wl(window.wl_surface());
        let window_id = surface_data.window_id;

        let Some(window_inner) = self.window_by_id(window_id) else {
            log::warn!("Ignoring Wayland window event for unknown window id {window_id}");
            return;
        };

        let p = window_inner.borrow().pending_event.clone();
        let mut pending_event = match p.lock() {
            Ok(pending_event) => pending_event,
            Err(_) => {
                log::warn!(
                    "Ignoring Wayland window event for window id {window_id}: pending event lock was poisoned"
                );
                return;
            }
        };

        let changed = match event {
            WaylandWindowEvent::Close => {
                // TODO: This should the new queue function
                // p.queue_close()
                if !pending_event.close {
                    pending_event.close = true;
                    true
                } else {
                    false
                }
            }
            WaylandWindowEvent::Request(configure) => {
                pending_event.window_configure.replace(configure.clone());
                // TODO: This should the new queue function
                // p.queue_configure(&configure)
                //
                let mut changed;
                pending_event.had_configure_event = true;
                if let (Some(w), Some(h)) = configure.new_size {
                    changed = pending_event.configure.is_none();
                    pending_event.configure.replace((w.get(), h.get()));
                } else {
                    changed = true;
                }

                let mut state = WindowState::default();
                if configure.state.contains(SCTKWindowState::FULLSCREEN) {
                    state |= WindowState::FULL_SCREEN;
                }
                if configure.state.contains(SCTKWindowState::MAXIMIZED) {
                    state |= WindowState::MAXIMIZED;
                }

                log::debug!(
                    "Config: self.window_state={:?}, states: {:?} {:?}",
                    pending_event.window_state,
                    state,
                    configure.state
                );

                if pending_event.window_state.is_none() && state != WindowState::default() {
                    changed = true;
                }

                pending_event.window_state.replace(state);
                changed
            }
        };
        if changed {
            WaylandConnection::with_window_inner(window_id, move |inner| {
                inner.dispatch_pending_event();
                Ok(())
            });
        }
    }
}

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _conn: &WConnection,
        _qh: &wayland_client::QueueHandle<Self>,
        _surface: &wayland_client::protocol::wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // We do nothing, we get the scale_factor from surface_data
    }

    fn frame(
        &mut self,
        _conn: &WConnection,
        _qh: &wayland_client::QueueHandle<Self>,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        _time: u32,
    ) {
        log::trace!("frame: CompositorHandler");
        let surface_data = SurfaceUserData::from_wl(surface);
        let window_id = surface_data.window_id;

        WaylandConnection::with_window_inner(window_id, |inner| {
            inner.next_frame_is_ready();
            Ok(())
        });
    }

    fn transform_changed(
        &mut self,
        _conn: &WConnection,
        _qh: &wayland_client::QueueHandle<Self>,
        _surface: &wayland_client::protocol::wl_surface::WlSurface,
        _new_transform: wayland_client::protocol::wl_output::Transform,
    ) {
        // TODO: do we need to do anything here?
    }

    fn surface_enter(
        &mut self,
        _conn: &WConnection,
        _qh: &wayland_client::QueueHandle<Self>,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        output: &wayland_client::protocol::wl_output::WlOutput,
    ) {
        let surface_data = SurfaceUserData::from_wl(surface);
        let window_id = surface_data.window_id;
        let output_name = self.output.info(output).map(|info| {
            info.name
                .clone()
                .unwrap_or_else(|| format!("{} {}", info.model, info.make))
        });

        if let Some(output_name) = output_name {
            WaylandConnection::with_window_inner(window_id, move |inner| {
                if !inner.active_output_names.contains(&output_name) {
                    inner.active_output_names.push(output_name);
                }
                Ok(())
            });
        }
    }

    fn surface_leave(
        &mut self,
        _conn: &WConnection,
        _qh: &wayland_client::QueueHandle<Self>,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        output: &wayland_client::protocol::wl_output::WlOutput,
    ) {
        let surface_data = SurfaceUserData::from_wl(surface);
        let window_id = surface_data.window_id;
        let output_name = self.output.info(output).map(|info| {
            info.name
                .clone()
                .unwrap_or_else(|| format!("{} {}", info.model, info.make))
        });

        if let Some(output_name) = output_name {
            WaylandConnection::with_window_inner(window_id, move |inner| {
                inner
                    .active_output_names
                    .retain(|name| name != &output_name);
                Ok(())
            });
        }
    }
}

impl WindowHandler for WaylandState {
    fn request_close(
        &mut self,
        _conn: &WConnection,
        _qh: &wayland_client::QueueHandle<Self>,
        window: &XdgWindow,
    ) {
        self.handle_window_event(window, WaylandWindowEvent::Close);
    }

    fn configure(
        &mut self,
        _conn: &WConnection,
        _qh: &wayland_client::QueueHandle<Self>,
        window: &XdgWindow,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        self.handle_window_event(window, WaylandWindowEvent::Request(configure));
    }
}

impl Dispatch<OrgKdeKwinBlurManager, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &OrgKdeKwinBlurManager,
        _event: <OrgKdeKwinBlurManager as Proxy>::Event,
        _data: &GlobalData,
        _conn: &WConnection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        // No events from OrgKdeKwinBlurManager...
    }
}

impl Dispatch<OrgKdeKwinBlur, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &OrgKdeKwinBlur,
        _event: <OrgKdeKwinBlur as Proxy>::Event,
        _data: &GlobalData,
        _conn: &WConnection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        // No events from OrgKdeKwinBlur...
    }
}

impl Dispatch<WlRegion, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegion,
        _event: <WlRegion as Proxy>::Event,
        _data: &GlobalData,
        _conn: &WConnection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

pub(super) struct SurfaceUserData {
    surface_data: SurfaceData,
    pub(super) window_id: usize,
}

impl SurfaceUserData {
    pub(super) fn from_wl(wl: &WlSurface) -> &Self {
        wl.data()
            .expect("User data should be associated with WlSurface")
    }
    pub(super) fn try_from_wl(wl: &WlSurface) -> Option<&SurfaceUserData> {
        wl.data()
    }
}

impl SurfaceDataExt for SurfaceUserData {
    fn surface_data(&self) -> &SurfaceData {
        &self.surface_data
    }
}

impl HasDisplayHandle for WaylandWindowInner {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let Some(conn) = WaylandConnection::get() else {
            return Err(HandleError::Unavailable);
        };
        let conn = conn.wayland();
        let backend = conn.connection.backend();
        let handle = backend.display_handle()?;
        Ok(unsafe { DisplayHandle::borrow_raw(handle.as_raw()) })
    }
}

impl HasWindowHandle for WaylandWindowInner {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let Some(window) = self.window.as_ref() else {
            return Err(HandleError::Unavailable);
        };
        let Some(surface) = NonNull::new(window.wl_surface().id().as_ptr() as _) else {
            return Err(HandleError::Unavailable);
        };
        let handle = WaylandWindowHandle::new(surface);
        unsafe { Ok(WindowHandle::borrow_raw(RawWindowHandle::Wayland(handle))) }
    }
}

impl HasDisplayHandle for WaylandWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let Some(conn) = WaylandConnection::get() else {
            return Err(HandleError::Unavailable);
        };
        let conn = conn.wayland();
        let backend = conn.connection.backend();
        let handle = backend.display_handle()?;
        Ok(unsafe { DisplayHandle::borrow_raw(handle.as_raw()) })
    }
}

impl HasWindowHandle for WaylandWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let Some(conn) = Connection::get() else {
            return Err(HandleError::Unavailable);
        };
        let Some(handle) = conn.wayland().window_by_id(self.0) else {
            return Err(HandleError::Unavailable);
        };

        let inner = handle.borrow();
        let handle = inner.window_handle()?;
        unsafe { Ok(WindowHandle::borrow_raw(handle.as_raw())) }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        checked_pixel_dimensions, checked_surface_dimensions, compositor_repeat_transition,
        key_repeat_first_due, key_repeat_timer_plan, key_repeat_timing, key_repeat_window_event,
        new_pending_first_configure, read_pipe_with_timeout, resolve_pending_first_configure,
        validate_resize_increments, CompositorRepeatTransition, KeyRepeatAbort, KeyRepeatTimerPlan,
    };
    use crate::{
        Handled, KeyCode, KeyEvent, Modifiers, RawKeyEvent, ResizeIncrement, WindowEvent,
        WindowKeyEvent,
    };
    use futures_util::future::Abortable;
    use promise::BrokenPromise;
    use smithay_client_toolkit::data_device_manager::ReadPipe;
    use std::convert::TryFrom;
    use std::fs::File;
    use std::future::Future as StdFuture;
    use std::io::Write;
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    #[test]
    fn wayland_dimensions_reject_values_outside_the_signed_protocol_range() {
        const I32_MAX_U32: u32 = 2_147_483_647;
        const I32_OVERFLOW_U32: u32 = 2_147_483_648;

        assert_eq!(
            checked_surface_dimensions(I32_MAX_U32, 1),
            Some((i32::MAX, 1))
        );
        assert_eq!(checked_surface_dimensions(I32_OVERFLOW_U32, 1), None);
        assert_eq!(checked_surface_dimensions(0, 1), None);
        assert_eq!(checked_pixel_dimensions(I32_OVERFLOW_U32 as usize, 1), None);
        assert_eq!(checked_pixel_dimensions(1, 0), None);
    }

    #[test]
    fn wayland_resize_increments_reject_zero_divisors() {
        assert!(validate_resize_increments(ResizeIncrement::disabled()).is_ok());
        assert!(validate_resize_increments(ResizeIncrement {
            x: 0,
            y: 1,
            base_width: 0,
            base_height: 0,
        })
        .is_err());
        assert!(validate_resize_increments(ResizeIncrement {
            x: 1,
            y: 0,
            base_width: 0,
            base_height: 0,
        })
        .is_err());
    }
    use wezterm_input_types::KeyboardLedStatus;

    fn repeat_event() -> WindowKeyEvent {
        let raw = RawKeyEvent {
            key: KeyCode::RawCode(38),
            modifiers: Modifiers::NONE,
            leds: KeyboardLedStatus::empty(),
            phys_code: None,
            raw_code: 38,
            repeat_count: 1,
            key_is_down: true,
            handled: Handled::new(),
        };
        let key = KeyEvent {
            key: KeyCode::Char('a'),
            modifiers: Modifiers::NONE,
            leds: KeyboardLedStatus::empty(),
            repeat_count: 1,
            key_is_down: true,
            raw: Some(raw.clone()),
        };
        WindowKeyEvent::KeyEvent(key)
    }

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn key_repeat_timing_rejects_disabled_or_invalid_protocol_values() {
        assert_eq!(key_repeat_timing(-1, 400), None);
        assert_eq!(key_repeat_timing(0, 400), None);
        assert_eq!(key_repeat_timing(25, -1), None);
    }

    #[test]
    fn key_repeat_timing_is_positive_and_never_faster_than_requested() {
        assert_eq!(
            key_repeat_timing(25, 400),
            Some((Duration::from_millis(400), Duration::from_millis(40)))
        );
        assert_eq!(
            key_repeat_timing(25, i32::MAX)
                .expect("maximum protocol delay remains cancellable")
                .0,
            Duration::from_millis(u64::try_from(i32::MAX).expect("maximum delay is positive"))
        );

        for (rate, expected_gap_ms) in [(1, 1_000), (25, 40), (1_000, 1), (1_001, 1), (i32::MAX, 1)]
        {
            let (delay, gap) = key_repeat_timing(rate, 0).expect("positive rate must be enabled");
            assert_eq!(delay, Duration::ZERO);
            assert_eq!(gap, Duration::from_millis(expected_gap_ms));
            assert!(!gap.is_zero());
            let rate = u32::try_from(rate).expect("test rates are positive");
            assert!(gap.as_millis() * u128::from(rate) >= 1_000);
        }
    }

    #[test]
    fn key_repeat_timer_plan_preserves_absolute_phase_after_late_wakes() {
        let gap = Duration::from_millis(40);
        assert_eq!(
            key_repeat_timer_plan(Duration::ZERO, Duration::from_millis(400), gap),
            Some(KeyRepeatTimerPlan::Wait(Duration::from_millis(400)))
        );
        assert_eq!(
            key_repeat_timer_plan(Duration::from_millis(400), Duration::from_millis(400), gap,),
            Some(KeyRepeatTimerPlan::Dispatch {
                repeat_count: 1,
                next_due: Duration::from_millis(440),
            })
        );
        assert_eq!(
            key_repeat_timer_plan(Duration::from_millis(450), Duration::from_millis(440), gap,),
            Some(KeyRepeatTimerPlan::Dispatch {
                repeat_count: 1,
                next_due: Duration::from_millis(480),
            })
        );
        assert_eq!(
            key_repeat_timer_plan(Duration::from_millis(520), Duration::from_millis(480), gap,),
            Some(KeyRepeatTimerPlan::Dispatch {
                repeat_count: 2,
                next_due: Duration::from_millis(560),
            })
        );
    }

    #[test]
    fn key_repeat_timer_plan_saturates_and_stops_on_zero_gap() {
        let plan = key_repeat_timer_plan(
            Duration::from_secs(u64::from(u16::MAX) + 1),
            Duration::ZERO,
            Duration::from_millis(1),
        )
        .expect("positive gap must produce a finite plan");
        assert!(matches!(
            plan,
            KeyRepeatTimerPlan::Dispatch {
                repeat_count: u16::MAX,
                ..
            }
        ));
        assert_eq!(
            key_repeat_timer_plan(Duration::from_secs(1), Duration::ZERO, Duration::ZERO,),
            None
        );
    }

    #[test]
    fn repeat_event_updates_outer_and_nested_counts() {
        let WindowEvent::KeyEvent(key) = key_repeat_window_event(repeat_event(), 17) else {
            panic!("mapped repeat seed must remain a key event");
        };
        assert_eq!(key.repeat_count, 17);
        assert_eq!(key.raw.as_ref().map(|raw| raw.repeat_count), Some(17));
    }

    #[test]
    fn raw_repeat_seed_stays_raw_and_updates_its_count() {
        let WindowKeyEvent::KeyEvent(key) = repeat_event() else {
            unreachable!("fixture is a mapped key event");
        };
        let raw = key.raw.expect("mapped key fixture carries raw provenance");
        let WindowEvent::RawKeyEvent(raw) =
            key_repeat_window_event(WindowKeyEvent::RawKeyEvent(raw), 3)
        else {
            panic!("raw repeat seed must remain a raw event");
        };
        assert_eq!(raw.repeat_count, 3);
    }

    #[test]
    fn compositor_repeat_retains_matching_held_key_for_later_rate_handoff() {
        assert_eq!(
            compositor_repeat_transition(Some(7), 7),
            CompositorRepeatTransition::RetainHeld
        );
        assert_eq!(
            compositor_repeat_transition(Some(7), 8),
            CompositorRepeatTransition::RetireMismatched
        );
        assert_eq!(
            compositor_repeat_transition(None, 7),
            CompositorRepeatTransition::Untracked
        );
    }

    #[test]
    fn repeat_info_update_preserves_key_down_origin_without_replaying_old_backlog() {
        let elapsed = Duration::from_millis(750);
        let delay = Duration::from_millis(400);
        let faster_gap = Duration::from_millis(20);

        assert_eq!(
            key_repeat_first_due(elapsed, delay, faster_gap, None),
            Some(elapsed)
        );
        assert_eq!(
            key_repeat_first_due(elapsed, delay, faster_gap, Some(Duration::from_millis(740)),),
            Some(Duration::from_millis(760))
        );
        assert_eq!(
            key_repeat_first_due(elapsed, delay, faster_gap, Some(Duration::from_millis(400)),),
            Some(elapsed)
        );
        assert_eq!(
            key_repeat_first_due(Duration::from_millis(250), delay, faster_gap, None,),
            Some(delay)
        );
        // Once any repeat has been dispatched, a later repeat_info delay is
        // not a second initial-delay penalty; only the new gap controls the
        // handoff phase.
        assert_eq!(
            key_repeat_first_due(
                elapsed,
                Duration::from_millis(900),
                faster_gap,
                Some(Duration::from_millis(740)),
            ),
            Some(Duration::from_millis(760))
        );
    }

    #[test]
    fn key_repeat_abort_cancels_a_pending_detached_wait_without_sleeping() {
        let (abort, registration) = KeyRepeatAbort::new_pair();
        let mut pending = Box::pin(Abortable::new(std::future::pending::<()>(), registration));
        let waker = Waker::noop().clone();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(pending.as_mut().poll(&mut cx), Poll::Pending));

        drop(abort);

        assert!(matches!(
            pending.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
    }

    #[test]
    fn replacing_and_clearing_repeat_aborts_only_the_retired_lease() {
        let (predecessor, predecessor_registration) = KeyRepeatAbort::new_pair();
        let (successor, successor_registration) = KeyRepeatAbort::new_pair();
        let mut predecessor_wait = Box::pin(Abortable::new(
            std::future::pending::<()>(),
            predecessor_registration,
        ));
        let mut successor_wait = Box::pin(Abortable::new(
            std::future::pending::<()>(),
            successor_registration,
        ));
        let waker = Waker::noop().clone();
        let mut cx = Context::from_waker(&waker);
        let mut active = Some(predecessor);

        active.replace(successor);
        assert!(matches!(
            predecessor_wait.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        assert!(matches!(
            successor_wait.as_mut().poll(&mut cx),
            Poll::Pending
        ));

        active.take();
        assert!(matches!(
            successor_wait.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
    }

    #[test]
    fn pending_first_configure_future_resolves_after_notification() {
        let (promise, mut future) = new_pending_first_configure();
        let mut pending_first_configure = Some(promise);
        let waker = Waker::noop().clone();
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Pending
        ));

        resolve_pending_first_configure(&mut pending_first_configure);

        assert!(pending_first_configure.is_none());
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    #[test]
    fn pending_first_configure_resolution_is_idempotent() {
        let (promise, mut future) = new_pending_first_configure();
        let mut pending_first_configure = Some(promise);
        let waker = Waker::noop().clone();
        let mut cx = Context::from_waker(&waker);

        resolve_pending_first_configure(&mut pending_first_configure);
        resolve_pending_first_configure(&mut pending_first_configure);

        assert!(pending_first_configure.is_none());
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    #[test]
    fn pending_first_configure_drop_reports_broken_promise() {
        let (promise, mut future) = new_pending_first_configure();
        let waker = Waker::noop().clone();
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Pending
        ));

        drop(promise);

        match StdFuture::poll(Pin::new(&mut future), &mut cx) {
            Poll::Ready(Err(err)) => {
                assert!(err.downcast_ref::<BrokenPromise>().is_some());
            }
            other => panic!("expected Ready(Err(BrokenPromise)), got {:?}", other),
        }
    }

    #[test]
    fn read_pipe_with_timeout_reads_large_payload() {
        let (read_fd, write_fd) = pipe_pair();
        let payload = vec![b'y'; 128 * 1024];
        let writer_payload = payload.clone();
        let writer = std::thread::spawn(move || {
            let mut file = File::from(write_fd);
            file.write_all(&writer_payload).unwrap();
        });
        let read_pipe = unsafe { ReadPipe::from_raw_fd(read_fd.into_raw_fd()) };

        let text = read_pipe_with_timeout(read_pipe).unwrap();

        writer.join().unwrap();
        assert_eq!(text.as_bytes(), payload);
    }
}
