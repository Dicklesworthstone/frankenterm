use std::any::Any;
use std::cell::RefCell;
use std::cmp::max;
use std::convert::TryInto;
use std::io::{ErrorKind, Read};
use std::num::NonZeroU32;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use config::ConfigHandle;
use frankenterm_font::FontConfiguration;
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
use crate::x11::KeyboardWithFallback;
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
        self.dpi as f64 / crate::DEFAULT_DPI as f64
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

use super::copy_and_paste::{set_pipe_nonblocking, CopyAndPaste};
use super::pointer::{PendingMouse, PointerUserData};
use super::state::WaylandState;

#[derive(Debug)]
pub(super) struct KeyRepeatState {
    pub(super) when: Instant,
    pub(super) event: WindowKeyEvent,
}

impl KeyRepeatState {
    pub(super) fn schedule(state: Arc<Mutex<Self>>, window_id: usize) {
        promise::spawn::spawn_into_main_thread(async move {
            let delay;
            let gap;
            {
                let Some(conn) = WaylandConnection::get() else {
                    log::debug!(
                        "Stopping Wayland key repeat: connection unavailable while reading repeat settings"
                    );
                    return;
                };
                let conn = conn.wayland();
                let (rate, ddelay) = {
                    let wstate = conn.wayland_state.borrow();
                    (
                        wstate.key_repeat_rate as u64,
                        wstate.key_repeat_delay as u64,
                    )
                };
                if rate == 0 {
                    return;
                }
                delay = Duration::from_millis(ddelay);
                gap = Duration::from_millis(1000 / rate);
            }

            let mut initial = true;
            promise::spawn::sleep(delay).await;
            loop {
                {
                    let handle = {
                        let Some(conn) = WaylandConnection::get() else {
                            log::debug!(
                                "Stopping Wayland key repeat for window {window_id}: connection unavailable"
                            );
                            return;
                        };
                        let conn = conn.wayland();
                        match conn.window_by_id(window_id) {
                            Some(handle) => handle,
                            None => return,
                        }
                    };

                    let mut inner = handle.borrow_mut();

                    if inner.key_repeat.as_ref().map(|(_, k)| Arc::as_ptr(k))
                        != Some(Arc::as_ptr(&state))
                    {
                        // Key was released and/or some other key is doing
                        // its own repetition now
                        return;
                    }

                    let mut st = match state.lock() {
                        Ok(st) => st,
                        Err(_) => {
                            log::warn!(
                                "Stopping Wayland key repeat for window {window_id}: repeat state lock was poisoned"
                            );
                            inner.key_repeat.take();
                            return;
                        }
                    };

                    let mut repeat_count = 1;

                    let mut elapsed = st.when.elapsed();
                    if initial {
                        elapsed -= delay;
                        initial = false;
                    }

                    // If our scheduling interval is longer than the repeat
                    // gap, we need to inflate the repeat count to match
                    // the intended rate
                    while elapsed >= gap {
                        repeat_count += 1;
                        elapsed -= gap;
                    }

                    let event = match st.event.clone() {
                        WindowKeyEvent::KeyEvent(mut key) => {
                            key.repeat_count = repeat_count;
                            WindowEvent::KeyEvent(key)
                        }
                        WindowKeyEvent::RawKeyEvent(mut raw) => {
                            raw.repeat_count = repeat_count;
                            WindowEvent::RawKeyEvent(raw)
                        }
                    };

                    inner.events.dispatch(event);

                    st.when = Instant::now();
                }

                promise::spawn::sleep(gap).await;
            }
        })
        .detach();
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

        let window_id = conn.next_window_id();
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
        let hidden = match decor_mode {
            Some(DecorationMode::Client) => false,
            _ => true,
        };
        window_frame.set_hidden(hidden);
        if !hidden {
            window_frame.resize(
                NonZeroU32::new(dimensions.pixel_width as u32)
                    .ok_or_else(|| anyhow!("dimensions {dimensions:?} are invalid"))?,
                NonZeroU32::new(dimensions.pixel_height as u32)
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

        {
            let surface_to_pending = &mut conn.wayland_state.borrow_mut().surface_to_pending;
            surface_to_pending.insert(surface.id(), Arc::clone(&pending_mouse));
        }

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
        promise::spawn::spawn(async move {
            let Some(conn) = Connection::get() else {
                bail!("cannot enable OpenGL: Wayland connection unavailable");
            };
            if let Some(handle) = conn.wayland().window_by_id(window) {
                let mut inner = handle.borrow_mut();
                inner.enable_opengl()
            } else {
                anyhow::bail!("invalid window");
            }
        })
        .await
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
        WaylandConnection::with_window_inner(self.0, |inner| {
            inner.close();
            Ok(())
        });
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
            inner.set_resize_increments(incr)
        });
    }

    fn get_clipboard(&self, clipboard: Clipboard) -> Future<String> {
        let mut promise = Promise::new();
        let future = promise.get_future().unwrap();
        let promise = Arc::new(Mutex::new(promise));
        let window_future = WaylandConnection::with_window_inner(self.0, move |inner| {
            let read = match inner.copy_and_paste.lock() {
                Ok(mut copy_and_paste) => copy_and_paste.get_clipboard_data(clipboard)?,
                Err(_) => bail!("Wayland copy-and-paste lock was poisoned while reading clipboard"),
            };
            let promise_for_thread = Arc::clone(&promise);
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
                let mut promise =
                    lock_or_recover(&promise, "recording clipboard reader thread spawn failure");
                promise.err(anyhow!(
                    "unable to spawn Wayland clipboard reader thread: {err}"
                ));
            }
            Ok(())
        });
        let promise_on_error = Arc::clone(&promise);
        promise::spawn::spawn(async move {
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
        WaylandConnection::with_window_inner(self.0, move |inner| Ok(inner.maximize()));
    }

    fn restore(&self) {
        WaylandConnection::with_window_inner(self.0, move |inner| Ok(inner.restore()));
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
                Ok(size) if size == 0 => {
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
    pub(super) key_repeat: Option<(u32, Arc<Mutex<KeyRepeatState>>)>,
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
    fn close(&mut self) {
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

            wegl_surface = Some(WlEglSurface::new(
                object_id,
                self.dimensions.pixel_width as i32,
                self.dimensions.pixel_height as i32,
            )?);

            log::trace!("WEGL Surface here {:?}", wegl_surface);

            match wayland_conn.gl_connection.borrow().as_ref() {
                Some(glconn) => crate::egl::GlState::create_wayland_with_existing_connection(
                    glconn,
                    wegl_surface.as_ref().unwrap(),
                ),
                None => crate::egl::GlState::create_wayland(
                    Some(wayland_conn.connection.backend().display_ptr() as *const _),
                    wegl_surface.as_ref().unwrap(),
                ),
            }
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
            let factor = self.get_dpi_factor() as f64;

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

        if pending.configure.is_none() {
            if pending.dpi.is_some() {
                // Synthesize a pending configure event for the dpi change
                pending.configure.replace((
                    self.pixels_to_surface(self.dimensions.pixel_width as i32) as u32,
                    self.pixels_to_surface(self.dimensions.pixel_height as i32) as u32,
                ));
                log::debug!("synthesize configure with {:?}", pending.configure);
            }
        }

        if let Some(ref window_config) = pending.window_configure {
            self.window_frame.update_state(window_config.state);
            self.window_frame
                .update_wm_capabilities(window_config.capabilities);
        }

        if let Some((mut w, mut h)) = pending.configure.take() {
            log::trace!("Pending configure: w:{w}, h{h} -- {:?}", self.window);
            if self.window.is_some() {
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

                let mut pixel_width = self.surface_to_pixels(w.try_into().unwrap());
                let mut pixel_height = self.surface_to_pixels(h.try_into().unwrap());

                if self.window_state.can_resize() {
                    self.window_frame.set_resizable(true);
                    if let Some(incr) = self.resize_increments {
                        let min_width = incr.base_width + incr.x;
                        let min_height = incr.base_height + incr.y;
                        let extra_width = (pixel_width - incr.base_width as i32) % incr.x as i32;
                        let extra_height = (pixel_height - incr.base_height as i32) % incr.y as i32;
                        let desired_pixel_width = max(pixel_width - extra_width, min_width as i32);
                        let desired_pixel_height =
                            max(pixel_height - extra_height, min_height as i32);
                        w = self.pixels_to_surface(desired_pixel_width) as u32;
                        h = self.pixels_to_surface(desired_pixel_height) as u32;
                        pixel_width = self.surface_to_pixels(w.try_into().unwrap());
                        pixel_height = self.surface_to_pixels(h.try_into().unwrap());
                    }
                }

                log::trace!("Resizing frame");
                if !self.window_frame.is_hidden() {
                    // Clamp the size to at least one surface heigh/width.
                    let width = NonZeroU32::new(w).unwrap_or(NonZeroU32::new(1).unwrap());
                    let height = NonZeroU32::new(h).unwrap_or(NonZeroU32::new(1).unwrap());
                    self.window_frame.resize(width, height);
                    pending.refresh_decorations = true
                }
                let (x, y) = self.window_frame.location();
                let surface_width = self.pixels_to_surface(pixel_width);
                let surface_height = self.pixels_to_surface(pixel_height);
                self.window
                    .as_mut()
                    .unwrap()
                    .xdg_surface()
                    .set_window_geometry(x, y, surface_width, surface_height);
                // Compute the new pixel dimensions
                let new_dimensions = Dimensions {
                    pixel_width: pixel_width.try_into().unwrap(),
                    pixel_height: pixel_height.try_into().unwrap(),
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
        let active_surface_id = state.active_surface_id.borrow();
        let surface_id = surface.id();

        if let Some(active_surface_id) = active_surface_id.as_ref() {
            if surface_id == active_surface_id.clone() {
                if self.text_cursor.map(|prior| prior != rect).unwrap_or(true) {
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
        self.resize_increments.replace(incr);
        Ok(())
    }

    fn set_inner_size(&mut self, width: usize, height: usize) {
        let pixel_width = width as i32;
        let pixel_height = height as i32;
        let surface_width = self.pixels_to_surface(pixel_width) as u32;
        let surface_height = self.pixels_to_surface(pixel_height) as u32;
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

    /// Diagnostic accessor for the in-flight frame-callback count.
    /// Linux integration tests use this to assert the chain-depth
    /// invariant under the resize-storm reproducer the bead
    /// (ft-mpc9b.3.2) targets. The lifetime peak is also exposed
    /// for `ft doctor` once the GUI integration lands.
    pub(crate) fn frame_callback_chain_depth(&self) -> u32 {
        self.frame_callback_chain_depth
    }

    /// Lifetime peak of `frame_callback_chain_depth` since window
    /// construction.
    pub(crate) fn frame_callback_chain_depth_peak(&self) -> u32 {
        self.frame_callback_chain_depth_peak
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

    pub(crate) fn emit_focus(&mut self, mapper: &mut KeyboardWithFallback, focused: bool) {
        // Clear the modifiers when we change focus, otherwise weird
        // things can happen.  For instance, if we lost focus because
        // CTRL+SHIFT+N was pressed to spawn a new window, we'd be
        // left stuck with CTRL+SHIFT held down and the window would
        // be left in a broken state.

        self.modifiers = Modifiers::NONE;
        mapper.update_modifier_state(0, 0, 0, 0);
        self.key_repeat.take();
        self.events.dispatch(WindowEvent::FocusChanged(focused));
        self.text_cursor.take();
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
        mapper: &mut KeyboardWithFallback,
        event: WlKeyboardEvent,
    ) {
        match event {
            WlKeyboardEvent::Enter { keys, .. } => {
                let key_codes = keys
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
                    .collect::<Vec<_>>();
                log::trace!("keyboard event: Enter with keys: {:?}", key_codes);
                self.emit_focus(mapper, true);
            }
            WlKeyboardEvent::Leave { .. } => {
                self.emit_focus(mapper, false);
            }
            WlKeyboardEvent::Key { key, state, .. } => {
                let pressed = match state.into_result() {
                    Ok(KeyState::Pressed) => true,
                    Ok(KeyState::Released) => false,
                    Ok(key_state) => {
                        log::warn!("Ignoring Wayland keyboard event with unsupported key state {key_state:?}");
                        return;
                    }
                    Err(raw_state) => {
                        log::warn!(
                            "Ignoring Wayland keyboard event with unknown key state {raw_state:?}"
                        );
                        return;
                    }
                };
                if let Some(event) = mapper.process_wayland_key(key, pressed, &mut self.events) {
                    let rep = Arc::new(Mutex::new(KeyRepeatState {
                        when: Instant::now(),
                        event,
                    }));
                    self.key_repeat.replace((key, Arc::clone(&rep)));
                    let window_id = SurfaceUserData::from_wl(
                        self.window
                            .as_ref()
                            .expect("window should exist")
                            .wl_surface(),
                    )
                    .window_id;
                    KeyRepeatState::schedule(rep, window_id);
                } else if let Some((cur_key, _)) = self.key_repeat.as_ref() {
                    // important to check that it's the same key, because the release of the previously
                    // repeated key can come right after the press of the newly held key
                    if *cur_key == key {
                        self.key_repeat.take();
                    }
                }
            }
            WlKeyboardEvent::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                mapper.update_modifier_state(mods_depressed, mods_latched, mods_locked, group);

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
        let pointer_data = pointer.data::<PointerUserData>().unwrap();
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

#[cfg(test)]
mod tests {
    use super::{
        new_pending_first_configure, read_pipe_with_timeout, resolve_pending_first_configure,
    };
    use promise::BrokenPromise;
    use smithay_client_toolkit::data_device_manager::ReadPipe;
    use std::fs::File;
    use std::future::Future as StdFuture;
    use std::io::Write;
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
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
            other => panic!("expected Ready(Err(BrokenPromise)), got {other:?}"),
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
