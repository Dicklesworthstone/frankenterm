use async_trait::async_trait;
use bitflags::bitflags;
use config::window::WindowLevel;
use config::{ConfigHandle, Dimension, GeometryOrigin};
use promise::Future;
use std::any::Any;
use std::path::PathBuf;
use std::rc::Rc;
use thiserror::Error;
use url::Url;
pub mod bitmaps;
pub use wezterm_color_types as color;
mod configuration;
pub mod connection;
pub mod os;
pub mod screen;
mod spawn;

pub use raw_window_handle;

#[cfg(target_os = "macos")]
pub(crate) const DEFAULT_DPI: f64 = 72.0;
#[cfg(not(target_os = "macos"))]
pub(crate) const DEFAULT_DPI: f64 = 96.0;

pub fn default_dpi() -> f64 {
    match Connection::get() {
        Some(conn) => conn.default_dpi(),
        None => DEFAULT_DPI,
    }
}

pub(crate) fn reserve_window_main_thread(
    service_class: promise::spawn::MainThreadServiceClass,
    estimated_bytes: usize,
    operation: &'static str,
) -> Result<
    promise::spawn::MainThreadSpawnReservation,
    Box<promise::spawn::MainThreadReservationOutcome>,
> {
    match promise::spawn::try_reserve_main_thread(service_class, estimated_bytes) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
            metrics::counter!(
                "window.main_thread_admission",
                "operation" => operation,
                "outcome" => "admitted"
            )
            .increment(1);
            Ok(reservation)
        }
        rejected => {
            metrics::counter!(
                "window.main_thread_admission",
                "operation" => operation,
                "outcome" => "terminal_rejection"
            )
            .increment(1);
            log::error!(
                "main-thread scheduler rejected window operation {operation} before task construction: {rejected:?}"
            );
            Err(Box::new(rejected))
        }
    }
}

mod egl;

// Tests that install a promise main-thread scheduler share process-global
// state even when their native windows/connections are otherwise isolated.
#[cfg(test)]
pub(crate) static REPAINT_SCHEDULER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Finish in the already admitted Render task. Re-admitting completion as a
/// window operation can strand the repaint latch when the general pool is full.
/// Paint time and time queued before the first poll both count toward the cap.
pub(crate) async fn complete_repaint_after_interval(
    paint_started: std::time::Instant,
    interval: std::time::Duration,
    complete: impl FnOnce(),
) {
    let remaining = interval.saturating_sub(paint_started.elapsed());
    if !remaining.is_zero() {
        promise::spawn::sleep(remaining).await;
    }
    complete();
}

pub use bitmaps::{BitmapImage, Image};
pub use connection::*;
pub use glium;
pub use os::*;
pub use wezterm_input_types::*;

/// A conservative prefilter, not a replacement for AppKit key-equivalent
/// matching. AppKit still decides whether the original event activates an
/// enabled menu item. Ignoring Shift here admits both a printed symbol and its
/// unshifted key without imposing a US keyboard layout.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn font_menu_key_candidate(
    characters: &str,
    unshifted_characters: &str,
    modifiers: Modifiers,
    equivalent: &str,
    equivalent_modifiers: Modifiers,
) -> bool {
    !equivalent.is_empty()
        && (equivalent == characters || equivalent == unshifted_characters)
        && (modifiers.remove_positional_mods() - Modifiers::SHIFT)
            == (equivalent_modifiers.remove_positional_mods() - Modifiers::SHIFT)
}

/// Try an existing menu prefix in order. None means its owner or attachment
/// changed during synchronous native dispatch, so abandon the accelerator.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn dispatch_key_equivalent_prefix<T>(
    menus: &[T],
    last: usize,
    mut dispatch: impl FnMut(usize, &T) -> Option<bool>,
) -> bool {
    if last >= menus.len() {
        return false;
    }
    for (index, menu) in menus[..=last].iter().enumerate() {
        match dispatch(index, menu) {
            Some(true) => return true,
            Some(false) => {}
            None => return false,
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clipboard {
    #[default]
    Clipboard,
    PrimarySelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: usize,
}

pub type ULength = euclid::Length<usize, PixelUnit>;
pub type Rect = euclid::Rect<isize, PixelUnit>;
pub type RectF = euclid::Rect<f32, PixelUnit>;
pub type Size = euclid::Size2D<isize, PixelUnit>;
pub type SizeF = euclid::Size2D<f32, PixelUnit>;
pub type ScreenRect = euclid::Rect<isize, ScreenPixelUnit>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseCursor {
    Arrow,
    Hand,
    Text,
    SizeUpDown,
    SizeLeftRight,
}

/// Represents the preferred appearance of the windowing
/// environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Appearance {
    /// Standard dark-text-on-light-background presentation
    Light,
    /// Dark mode, with predominantly dark or muted colors
    Dark,
    /// dark-text-on-light-background, but in a higher contrast
    /// more accesible palette
    LightHighContrast,
    /// darker background but with higher contrast than regular
    /// dark mode
    DarkHighContrast,
}

impl std::fmt::Display for Appearance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::LightHighContrast => "LightHighContrast",
            Self::DarkHighContrast => "DarkHighContrast",
        };
        f.write_str(name)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct WindowState: u8 {
        /// Occupies the whole screen; cannot be resized while in this state.
        const FULL_SCREEN = 1<<1;
        /// Maximized along either or both of horizontal or vertical dimensions;
        /// cannot be resized while in this state.
        const MAXIMIZED = 1<<2;
        /// Minimized or in some kind of off-screen state. Cannot be repainted
        /// while in this state.
        const HIDDEN = 1<<3;
        /// Always on top (floating) window
        const ALWAYS_ON_TOP = 1<<4;
        /// Always on bottom (docked) window
        const ALWAYS_ON_BOTTOM = 1<<5;
    }
}

impl WindowState {
    pub fn can_resize(self) -> bool {
        !self.intersects(Self::FULL_SCREEN | Self::MAXIMIZED)
    }

    pub fn can_paint(self) -> bool {
        !self.contains(Self::HIDDEN)
    }

    pub fn as_window_level(self) -> WindowLevel {
        if self.contains(Self::ALWAYS_ON_TOP) {
            WindowLevel::AlwaysOnTop
        } else if self.contains(Self::ALWAYS_ON_BOTTOM) {
            WindowLevel::AlwaysOnBottom
        } else {
            WindowLevel::Normal
        }
    }
}

#[derive(Debug, Clone)]
pub enum WindowKeyEvent {
    RawKeyEvent(RawKeyEvent),
    KeyEvent(KeyEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadKeyStatus {
    /// Not in a dead key processing hold
    None,
    /// Holding until composition is done; the string is the uncommitted
    /// composition text to show as a placeholder
    Composing(String),
}

#[derive(Debug)]
pub enum WindowEvent {
    /// Called when the window close button is clicked.
    /// The window closure is deferred and this event is
    /// sent to your application to decide whether it will
    /// really close the window.
    CloseRequested,

    /// Called when the window is being destroyed by the window system
    Destroyed,

    /// Called when the window has been resized
    Resized {
        dimensions: Dimensions,
        window_state: WindowState,
        live_resizing: bool,
    },

    /// Called when a program-requested set_inner_size() has finished
    SetInnerSizeCompleted,

    /// Called when the window has been invalidated and needs to
    /// be repainted
    NeedRepaint,

    /// Called when the window gains/loses focus
    FocusChanged(bool),

    AdviseDeadKeyStatus(DeadKeyStatus),

    /// Called to handle a raw key event, prior to any dead key,
    /// keymap composition or other higher level treatment.
    /// If you handle this key event, you must call
    /// event.set_handled() to prevent additional processing.
    RawKeyEvent(RawKeyEvent),

    /// Called to handle a key event.
    KeyEvent(KeyEvent),

    MouseEvent(MouseEvent),
    MouseLeave,

    AppearanceChanged(Appearance),

    Notification(Box<dyn Any + Send + Sync>),

    // Called when the files are being dragged into the window
    DraggedFile(Vec<PathBuf>),

    // Called when the files are dropped into the window
    DroppedFile(Vec<PathBuf>),

    // Called when urls are dropped into the window
    DroppedUrl(Vec<Url>),

    // Called when text is dropped into the window
    DroppedString(String),

    /// Called by menubar dispatching stuff on some systems
    PerformKeyAssignment(config::keyassignment::KeyAssignment),

    AdviseModifiersLedStatus(Modifiers, KeyboardLedStatus),
}

type WindowEventHandler = dyn FnMut(WindowEvent, &Window);

pub struct WindowEventSender {
    handler: Box<WindowEventHandler>,
    window: Option<Window>,
}

impl WindowEventSender {
    pub fn new<F: 'static + FnMut(WindowEvent, &Window)>(handler: F) -> Self {
        Self {
            handler: Box::new(handler),
            window: None,
        }
    }

    pub(crate) fn assign_window(&mut self, window: Window) {
        self.window.replace(window);
    }

    pub fn dispatch(&mut self, event: WindowEvent) {
        if let Some(window) = self.window.as_ref() {
            log::trace!("{:?}", event);
            (self.handler)(event, window);
        }
    }
}

#[derive(Debug, Error)]
#[error("Graphics drivers lost context")]
pub struct GraphicsDriversLostContext {}

#[async_trait(?Send)]
pub trait WindowOps {
    /// Show a hidden window
    fn show(&self);

    fn notify<T: Any + Send + Sync>(&self, t: T)
    where
        Self: Sized;

    /// Deliver a notification using capacity already owned by its producer.
    /// The exact scheduler generation and service class survive the handoff;
    /// this must never perform another fallible scheduler admission.
    fn notify_with_reservation<T: Any + Send + Sync>(
        &self,
        t: T,
        reservation: promise::spawn::MainThreadSpawnReservation,
    ) where
        Self: Sized;

    /// Setup opengl for rendering
    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>>;
    /// Advise the window that a frame is finished
    fn finish_frame(&self, frame: glium::Frame) -> anyhow::Result<()> {
        frame.finish()?;
        Ok(())
    }

    /// Hide a visible window
    fn hide(&self);

    /// Schedule the window to be closed
    fn close(&self);

    /// Change the cursor
    fn set_cursor(&self, cursor: Option<MouseCursor>);

    /// Invalidate the window so that the entire client area will
    /// be repainted shortly
    fn invalidate(&self);

    /// Change the titlebar text for the window
    fn set_title(&self, title: &str);

    /// Resize the inner or client area of the window
    fn set_inner_size(&self, width: usize, height: usize);

    /// Use for windows snap layouts
    fn set_maximize_button_position(&self, _rect: ScreenRect) {}

    /// Requests the windowing system to start a window drag.
    ///
    /// This is only implemented on backends that handle
    /// window movement on the server side (Wayland).
    fn request_drag_move(&self) {}

    /// Signal to the windowing system that the mouse is over
    /// a window dragging area.
    ///
    /// This is only implemented on backends that need to
    /// know if the mouse is in a drag area to handle the
    /// click before forwarding the event (Windows).
    fn set_window_drag_position(&self, _coords: ScreenPoint) {}

    /// Changes the location of the window on the screen.
    /// The coordinates are of the top left pixel of the
    /// client area.
    ///
    /// This is only implemented on backends that allow
    /// windows to move themselves (not Wayland).
    fn set_window_position(&self, _coords: ScreenPoint) {}

    /// inform the windowing system of the current textual
    /// cursor input location.  This is used primarily for
    /// the platform specific input method editor
    fn set_text_cursor_position(&self, _cursor: Rect) {}

    /// Initiate textual transfer from the clipboard
    fn get_clipboard(&self, clipboard: Clipboard) -> Future<String>;

    /// Set some text in the clipboard
    fn set_clipboard(&self, clipboard: Clipboard, text: String);

    /// Set window level. Depending on the environment and user preferences
    fn set_window_level(&self, _level: WindowLevel) {}

    /// Set the icon for the window.
    /// Depending on the system this may be shown in its titlebar
    /// and/or in the task manager/task switcher
    fn set_icon(&self, _image: Image) {}

    fn maximize(&self) {}
    fn restore(&self) {}
    fn focus(&self) {}

    fn toggle_fullscreen(&self) {}

    fn config_did_change(&self, _config: &config::ConfigHandle) {}

    /// Configure the Window so that the desktop environment
    /// will constrain resizes so that they are multiples of
    /// the x and y values specified.
    /// This may not be supported or respected by the desktop
    /// environment.
    fn set_resize_increments(&self, _incr: ResizeIncrement) {}

    fn get_os_parameters(
        &self,
        _config: &ConfigHandle,
        _window_state: WindowState,
    ) -> anyhow::Result<Option<os::parameters::Parameters>> {
        Ok(None)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RequestedWindowGeometry {
    pub width: Dimension,
    pub height: Dimension,
    pub x: Option<Dimension>,
    pub y: Option<Dimension>,
    /// Specifies basis for evaluating x/y coords.
    /// Also applies to width/height when computing % based dimensions
    pub origin: GeometryOrigin,
}

#[derive(Debug, Clone)]
pub struct ResolvedGeometry {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ResizeIncrement {
    pub x: u16,
    pub y: u16,
    pub base_width: u16,
    pub base_height: u16,
}

impl ResizeIncrement {
    /// Use this as a readable shorthand for disabling the feature
    pub fn disabled() -> Self {
        Self {
            x: 1,
            y: 1,
            base_width: 0,
            base_height: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overdue_repaint_completes_with_no_free_general_admission() {
        use promise::spawn::{
            MainThreadAdmissionLimits, MainThreadReservationOutcome, MainThreadServiceClass,
            SimpleExecutor,
        };
        use std::cell::Cell;
        use std::time::{Duration, Instant};

        let _scheduler_guard = REPAINT_SCHEDULER_TEST_LOCK.lock().unwrap();

        // One general slot is occupied by the Render timer itself; the other
        // slot is reserved for critical input/topology and cannot admit the old
        // Interactive completion. Run the real timer future in that condition.
        let limits = MainThreadAdmissionLimits::new(2, 16 * 1024, 1, 8 * 1024).unwrap();
        let executor = SimpleExecutor::try_with_limits(limits).unwrap();
        let reservation = reserve_window_main_thread(
            MainThreadServiceClass::Render,
            4 * 1024,
            "repaint saturation regression",
        )
        .unwrap();
        let throttled = Rc::new(Cell::new(true));
        let completion = Rc::clone(&throttled);
        let started = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        reservation
            .spawn_local(complete_repaint_after_interval(
                started,
                Duration::from_millis(33),
                move || {
                    assert!(matches!(
                        promise::spawn::try_reserve_main_thread(
                            MainThreadServiceClass::Interactive,
                            4 * 1024,
                        ),
                        MainThreadReservationOutcome::RetryableFull(_)
                    ));
                    completion.set(false);
                },
            ))
            .detach();

        assert!(executor.try_tick().unwrap());
        assert!(
            !throttled.get(),
            "an overdue repaint must finish on its first poll without a new admission or timer"
        );
        assert!(!executor.try_tick().unwrap());
        // Completion must also release its original permit.
        assert!(reserve_window_main_thread(
            MainThreadServiceClass::Interactive,
            4 * 1024,
            "repaint completion capacity returned",
        )
        .is_ok());
    }

    #[test]
    fn repaint_completion_waits_when_its_interval_has_not_elapsed() {
        use std::cell::Cell;
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        use std::time::{Duration, Instant};

        let completed = Cell::new(false);
        // Poll only once and then cancel; no wall-clock sleep is needed to
        // prove that a fresh timer cannot immediately release the frame cap.
        let mut timer = Box::pin(complete_repaint_after_interval(
            Instant::now(),
            Duration::from_secs(3600),
            || completed.set(true),
        ));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(timer.as_mut().poll(&mut context), Poll::Pending));
        drop(timer);
        assert!(!completed.get());
    }

    #[test]
    fn font_menu_candidates_follow_live_remaps_and_preserve_modifiers() {
        let cmd = Modifiers::SUPER;
        let shifted = cmd | Modifiers::SHIFT;
        let alt = cmd | Modifiers::ALT;
        for (text, base, mods, key, key_mods, expected) in [
            ("-", "-", cmd, "-", cmd, true),
            ("+", "=", shifted, "=", cmd, true),
            ("é", "é", cmd, "é", cmd, true),
            ("j", "j", alt, "j", alt, true),
            ("-", "-", cmd, "j", cmd, false),
            ("j", "j", cmd, "j", alt, false),
            ("j", "j", Modifiers::CTRL, "j", cmd, false),
            ("", "", cmd, "", cmd, false),
            ("`", "`", cmd, "-", cmd, false),
        ] {
            assert_eq!(
                font_menu_key_candidate(text, base, mods, key, key_mods),
                expected,
                "event={text:?}/{mods:?}, live menu={key:?}/{key_mods:?}",
            );
        }
    }

    #[test]
    fn font_menu_prefix_preserves_collisions_and_never_visits_later_windows_menu() {
        let menus = ["Application", "Shell", "Edit", "View", "Window"];
        for winner in [0, 2, 3] {
            let mut visited = vec![];
            assert!(dispatch_key_equivalent_prefix(&menus, 3, |index, menu| {
                visited.push(*menu);
                Some(index == winner)
            }));
            assert_eq!(visited, menus[..=winner]);
        }
        let mut visited = vec![];
        assert!(!dispatch_key_equivalent_prefix(&menus, 3, |_, menu| {
            visited.push(*menu);
            Some(false)
        }));
        assert_eq!(visited, menus[..=3]);
    }

    #[test]
    fn font_menu_prefix_abandons_changed_owner_or_missing_target() {
        let mut visited = vec![];
        assert!(!dispatch_key_equivalent_prefix(
            &[0, 1, 2, 3],
            2,
            |index, _| {
                visited.push(index);
                if index == 1 {
                    None
                } else {
                    Some(false)
                }
            }
        ));
        assert_eq!(visited, [0, 1]);
        assert!(!dispatch_key_equivalent_prefix(&[0], 1, |_, _| {
            panic!("invalid menu prefix must not dispatch")
        }));
    }

    #[test]
    fn window_state_capability_and_level_goldens() {
        struct Case {
            name: &'static str,
            state: WindowState,
            can_resize: bool,
            can_paint: bool,
            level: WindowLevel,
        }

        let cases = [
            Case {
                name: "normal",
                state: WindowState::default(),
                can_resize: true,
                can_paint: true,
                level: WindowLevel::Normal,
            },
            Case {
                name: "full screen",
                state: WindowState::FULL_SCREEN,
                can_resize: false,
                can_paint: true,
                level: WindowLevel::Normal,
            },
            Case {
                name: "maximized",
                state: WindowState::MAXIMIZED,
                can_resize: false,
                can_paint: true,
                level: WindowLevel::Normal,
            },
            Case {
                name: "hidden",
                state: WindowState::HIDDEN,
                can_resize: true,
                can_paint: false,
                level: WindowLevel::Normal,
            },
            Case {
                name: "always on top wins over bottom",
                state: WindowState::ALWAYS_ON_BOTTOM | WindowState::ALWAYS_ON_TOP,
                can_resize: true,
                can_paint: true,
                level: WindowLevel::AlwaysOnTop,
            },
        ];

        for case in cases {
            assert_eq!(
                case.state.can_resize(),
                case.can_resize,
                "{} resize",
                case.name
            );
            assert_eq!(
                case.state.can_paint(),
                case.can_paint,
                "{} paint",
                case.name
            );
            assert_eq!(
                case.state.as_window_level(),
                case.level,
                "{} level",
                case.name
            );
        }
    }

    #[test]
    fn disabled_resize_increment_uses_one_cell_increments_without_base_size() {
        let disabled = ResizeIncrement::disabled();

        assert_eq!(disabled.x, 1);
        assert_eq!(disabled.y, 1);
        assert_eq!(disabled.base_width, 0);
        assert_eq!(disabled.base_height, 0);
    }
}
