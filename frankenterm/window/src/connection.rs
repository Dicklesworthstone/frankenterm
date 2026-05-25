use crate::screen::Screens;
use crate::{Appearance, Connection, GeometryOrigin, RequestedWindowGeometry, ResolvedGeometry};
use anyhow::Result as Fallible;
use anyhow::anyhow;
use config::DimensionContext;
use config::keyassignment::KeyAssignment;
use promise::{Future, Promise};
use std::cell::RefCell;
use std::fmt::Display;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};

thread_local! {
    static CONN: RefCell<Option<Rc<Connection>>> = const { RefCell::new(None) };
}

fn nop_event_handler(_event: ApplicationEvent) {}

static EVENT_HANDLER: Mutex<fn(ApplicationEvent)> = Mutex::new(nop_event_handler);

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(crate) fn new_window_op_promise<T>() -> (Promise<T>, Future<T>)
where
    T: Send + 'static,
{
    let mut promise = Promise::new();
    let future = promise
        .get_future()
        .expect("window operation promise should always create a future");
    (promise, future)
}

pub(crate) fn fail_window_op_for_destroyed_window<T>(
    promise: &mut Promise<T>,
    platform: &'static str,
    window_id: impl Display,
) where
    T: Send + 'static,
{
    promise.err(anyhow!("{platform} window {window_id} has been destroyed"));
}

pub fn shutdown() {
    CONN.with(|m| drop(m.borrow_mut().take()));
}

#[derive(Debug)]
pub enum ApplicationEvent {
    /// The system wants to open a command in the terminal
    OpenCommandScript(String),
    PerformKeyAssignment(KeyAssignment),
}

pub trait ConnectionOps {
    fn get() -> Option<Rc<Connection>> {
        let mut res = None;
        CONN.with(|m| {
            if let Some(mux) = &*m.borrow() {
                res = Some(Rc::clone(mux));
            }
        });
        res
    }

    fn name(&self) -> String;

    fn set_event_handler(&self, func: fn(ApplicationEvent)) {
        let mut handler = lock_or_recover(&EVENT_HANDLER);
        *handler = func;
    }

    fn dispatch_app_event(&self, event: ApplicationEvent) {
        let func = *lock_or_recover(&EVENT_HANDLER);
        func(event);
    }

    fn default_dpi(&self) -> f64 {
        crate::DEFAULT_DPI
    }

    fn init() -> Fallible<Rc<Connection>> {
        let conn = Rc::new(Connection::create_new()?);
        CONN.with(|m| *m.borrow_mut() = Some(Rc::clone(&conn)));
        crate::spawn::SPAWN_QUEUE.register_promise_schedulers();
        Ok(conn)
    }

    fn terminate_message_loop(&self);
    fn run_message_loop(&self) -> Fallible<()>;

    /// Retrieve the current appearance for the application.
    fn get_appearance(&self) -> Appearance {
        Appearance::Light
    }

    /// Hide the application.
    /// This actions hides all of the windows of the application and switches
    /// focus away from it.
    fn hide_application(&self) {}

    /// Perform the system beep/notification sound
    fn beep(&self) {}

    /// Returns information about the screens
    fn screens(&self) -> anyhow::Result<Screens> {
        anyhow::bail!("Unable to query screen information");
    }

    fn resolve_geometry(&self, geometry: RequestedWindowGeometry) -> ResolvedGeometry {
        let bounds = match self.screens() {
            Ok(screens) => {
                log::trace!("{screens:?}");

                match geometry.origin {
                    GeometryOrigin::ScreenCoordinateSystem => screens.virtual_rect,
                    GeometryOrigin::MainScreen => screens.main.rect,
                    GeometryOrigin::ActiveScreen => screens.active.rect,
                    GeometryOrigin::Named(name) => match screens.by_name.get(&name) {
                        Some(info) => info.rect,
                        None => {
                            log::error!(
                                "Requested display {} was not found; available displays are: {:?}. \
                             Using primary display instead",
                                name,
                                screens.by_name,
                            );
                            screens.main.rect
                        }
                    },
                }
            }
            Err(_) => euclid::rect(0, 0, 65535, 65535),
        };

        let dpi = self.default_dpi();
        let width_context = DimensionContext {
            dpi: dpi as f32,
            pixel_max: bounds.width() as f32,
            pixel_cell: bounds.width() as f32,
        };
        let height_context = DimensionContext {
            dpi: dpi as f32,
            pixel_max: bounds.height() as f32,
            pixel_cell: bounds.height() as f32,
        };
        let width = geometry.width.evaluate_as_pixels(width_context) as usize;
        let height = geometry.height.evaluate_as_pixels(height_context) as usize;
        let x = geometry
            .x
            .map(|x| x.evaluate_as_pixels(width_context) as i32 + bounds.origin.x as i32);
        let y = geometry
            .y
            .map(|y| y.evaluate_as_pixels(height_context) as i32 + bounds.origin.y as i32);

        ResolvedGeometry {
            x,
            y,
            width,
            height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationEvent, ConnectionOps, EVENT_HANDLER, Fallible, RequestedWindowGeometry,
        fail_window_op_for_destroyed_window, new_window_op_promise, nop_event_handler,
    };
    use crate::screen::{ScreenInfo, Screens};
    use config::{Dimension, GeometryOrigin};
    use std::collections::HashMap;
    use std::future::Future as StdFuture;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Waker};

    static EVENT_HANDLER_TEST_CALLED: AtomicBool = AtomicBool::new(false);

    struct TestConnection;

    impl ConnectionOps for TestConnection {
        fn name(&self) -> String {
            "test".to_string()
        }

        fn terminate_message_loop(&self) {}

        fn run_message_loop(&self) -> Fallible<()> {
            Ok(())
        }
    }

    struct GeometryConnection {
        screens: Option<Screens>,
        dpi: f64,
    }

    impl ConnectionOps for GeometryConnection {
        fn name(&self) -> String {
            "geometry-test".to_string()
        }

        fn default_dpi(&self) -> f64 {
            self.dpi
        }

        fn screens(&self) -> anyhow::Result<Screens> {
            self.screens
                .clone()
                .ok_or_else(|| anyhow::anyhow!("screen query unavailable"))
        }

        fn terminate_message_loop(&self) {}

        fn run_message_loop(&self) -> Fallible<()> {
            Ok(())
        }
    }

    fn screen_info(name: &str, x: isize, y: isize, width: isize, height: isize) -> ScreenInfo {
        ScreenInfo {
            name: name.to_string(),
            rect: euclid::rect(x, y, width, height),
            scale: 1.0,
            max_fps: None,
            effective_dpi: None,
        }
    }

    fn test_screens() -> Screens {
        let main = screen_info("main", 0, 0, 1920, 1080);
        let active = screen_info("active", 100, 200, 800, 600);
        let named = screen_info("sidecar", -1200, 50, 1200, 900);
        let by_name = HashMap::from([(named.name.clone(), named)]);

        Screens {
            main,
            active,
            by_name,
            virtual_rect: euclid::rect(-1200, 0, 3120, 1100),
        }
    }

    fn reentrant_event_handler(_event: ApplicationEvent) {
        assert!(EVENT_HANDLER.try_lock().is_ok());
        EVENT_HANDLER_TEST_CALLED.store(true, Ordering::SeqCst);
    }

    #[test]
    fn resolve_geometry_uses_requested_origin_bounds_and_offsets() {
        struct Case {
            name: &'static str,
            origin: GeometryOrigin,
            expected_x: Option<i32>,
            expected_y: Option<i32>,
            expected_width: usize,
            expected_height: usize,
        }

        let cases = [
            Case {
                name: "active screen offsets percent x and point height",
                origin: GeometryOrigin::ActiveScreen,
                expected_x: Some(300),
                expected_y: Some(212),
                expected_width: 400,
                expected_height: 144,
            },
            Case {
                name: "named screen applies negative origin",
                origin: GeometryOrigin::Named("sidecar".to_string()),
                expected_x: Some(-900),
                expected_y: Some(62),
                expected_width: 600,
                expected_height: 144,
            },
            Case {
                name: "missing named screen falls back to main",
                origin: GeometryOrigin::Named("missing".to_string()),
                expected_x: Some(480),
                expected_y: Some(12),
                expected_width: 960,
                expected_height: 144,
            },
        ];

        let conn = GeometryConnection {
            screens: Some(test_screens()),
            dpi: 144.0,
        };

        for case in cases {
            let resolved = conn.resolve_geometry(RequestedWindowGeometry {
                width: Dimension::Percent(0.5),
                height: Dimension::Points(72.0),
                x: Some(Dimension::Percent(0.25)),
                y: Some(Dimension::Pixels(12.9)),
                origin: case.origin,
            });

            assert_eq!(resolved.x, case.expected_x, "{} x", case.name);
            assert_eq!(resolved.y, case.expected_y, "{} y", case.name);
            assert_eq!(resolved.width, case.expected_width, "{} width", case.name);
            assert_eq!(
                resolved.height, case.expected_height,
                "{} height",
                case.name
            );
        }
    }

    #[test]
    fn resolve_geometry_falls_back_to_virtual_canvas_when_screen_query_fails() {
        let conn = GeometryConnection {
            screens: None,
            dpi: 96.0,
        };

        let resolved = conn.resolve_geometry(RequestedWindowGeometry {
            width: Dimension::Percent(0.5),
            height: Dimension::Pixels(600.9),
            x: None,
            y: None,
            origin: GeometryOrigin::ActiveScreen,
        });

        assert_eq!(resolved.x, None);
        assert_eq!(resolved.y, None);
        assert_eq!(resolved.width, 32767);
        assert_eq!(resolved.height, 600);
    }

    #[test]
    fn destroyed_window_operation_fails_closed() {
        let (mut promise, mut future) = new_window_op_promise::<()>();
        let waker = Waker::noop().clone();
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Pending
        ));

        fail_window_op_for_destroyed_window(&mut promise, "Wayland", 42);

        match StdFuture::poll(Pin::new(&mut future), &mut cx) {
            Poll::Ready(Err(err)) => {
                assert_eq!(err.to_string(), "Wayland window 42 has been destroyed");
            }
            other => panic!("expected Ready(Err), got {:?}", other),
        }
    }

    #[test]
    fn dispatch_app_event_drops_handler_lock_before_callback() {
        let conn = TestConnection;
        EVENT_HANDLER_TEST_CALLED.store(false, Ordering::SeqCst);
        conn.set_event_handler(reentrant_event_handler);

        conn.dispatch_app_event(ApplicationEvent::OpenCommandScript(String::new()));

        conn.set_event_handler(nop_event_handler);
        assert!(EVENT_HANDLER_TEST_CALLED.load(Ordering::SeqCst));
    }
}
