use super::clientpane::ConditionalInputDelivery;
use crate::client::{
    admit_interactive_rpc_now, validate_conditional_input_receipt, with_rpc_deadline, Client,
    ConditionalPaneInputSource, RpcGenerationScope,
};
use codec::*;
use mux::pane::PaneId;
use mux::PaneRegistrationHandle;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use wezterm_term::{MouseButton, MouseEvent, MouseEventKind};

const MOUSE_QUEUE_CAPACITY: usize = 256;

pub struct MouseState {
    worker_running: bool,
    queue: VecDeque<QueuedMouseEvent>,
    client: Client,
    remote_pane_id: PaneId,
}

struct QueuedMouseEvent {
    event: MouseEvent,
    scope: RpcGenerationScope,
    registration: PaneRegistrationHandle,
    source: Option<ConditionalPaneInputSource>,
    input_serial: InputSerial,
}

/// An admitted worker owns the entire queue until it drains. Cancellation must
/// retire its unsent entries instead of leaving an apparently active lane.
struct MouseWorkerGuard {
    state: Arc<Mutex<MouseState>>,
    drained: bool,
}

impl Drop for MouseWorkerGuard {
    fn drop(&mut self) {
        if !self.drained {
            let mut state = self.state.lock();
            let cancelled = state.queue.len();
            state.queue.clear();
            state.worker_running = false;
            metrics::counter!("mux.client.mouse_queue", "outcome" => "worker_cancelled")
                .increment(cancelled as u64);
        }
    }
}

fn coalesce_mouse_event(last: &mut MouseEvent, event: &MouseEvent) -> bool {
    if last.modifiers != event.modifiers || last.kind != event.kind {
        return false;
    }
    if last.kind == MouseEventKind::Move && last.button == event.button {
        *last = *event;
        return true;
    }
    // Wheel coordinates are observable in terminal mouse-reporting protocols.
    // Merge only equivalent positions, and retain a separate event on overflow.
    if last.x != event.x
        || last.y != event.y
        || last.x_pixel_offset != event.x_pixel_offset
        || last.y_pixel_offset != event.y_pixel_offset
    {
        return false;
    }
    let merged = match (&last.button, &event.button) {
        (MouseButton::WheelUp(a), MouseButton::WheelUp(b)) => {
            a.checked_add(*b).map(MouseButton::WheelUp)
        }
        (MouseButton::WheelDown(a), MouseButton::WheelDown(b)) => {
            a.checked_add(*b).map(MouseButton::WheelDown)
        }
        (MouseButton::WheelLeft(a), MouseButton::WheelLeft(b)) => {
            a.checked_add(*b).map(MouseButton::WheelLeft)
        }
        (MouseButton::WheelRight(a), MouseButton::WheelRight(b)) => {
            a.checked_add(*b).map(MouseButton::WheelRight)
        }
        _ => None,
    };
    if let Some(button) = merged {
        last.button = button;
        true
    } else {
        false
    }
}

impl MouseState {
    pub fn new(remote_pane_id: PaneId, client: Client) -> Self {
        Self {
            remote_pane_id,
            client,
            worker_running: false,
            queue: VecDeque::new(),
        }
    }

    pub fn enqueue(
        state: &Arc<Mutex<Self>>,
        event: MouseEvent,
        registration: PaneRegistrationHandle,
        scope: RpcGenerationScope,
        source: Option<ConditionalPaneInputSource>,
    ) -> anyhow::Result<()> {
        let mut mouse = state.lock();
        if !scope.is_available() || !scope.same_generation(&mouse.client.rpc_scope()) {
            metrics::counter!("mux.client.mouse_queue", "outcome" => "unavailable").increment(1);
            anyhow::bail!("cannot enqueue mouse input while mux RPC transport is unavailable");
        }
        if registration.try_with_current(|_| ()).is_none() {
            anyhow::bail!("cannot enqueue mouse input for a retired pane registration");
        }
        let input_serial = if let Some(source) = source {
            if !scope
                .agreed_codec_version()
                .is_some_and(|version| version >= CONDITIONAL_PANE_INPUT_V1_MIN_CODEC_VERSION)
                || scope.render_connection_identity().is_none_or(|identity| {
                    identity.session_incarnation != source.session_incarnation
                })
            {
                anyhow::bail!(
                    "mouse input identity does not belong to this supported mux connection"
                );
            }
            let input_serial = InputSerial::try_now()
                .ok_or_else(|| anyhow::anyhow!("process-local input serial space exhausted"))?;
            SendPaneInputConditionalV1 {
                pane_id: mouse.remote_pane_id,
                session_incarnation: source.session_incarnation,
                pane_registration: source.pane_registration,
                input_serial,
                effect: PaneInputEffectV1::Mouse(event),
            }
            .validate()?;
            input_serial
        } else {
            if !scope
                .agreed_codec_version()
                .is_some_and(|version| version < CONDITIONAL_PANE_INPUT_V1_MIN_CODEC_VERSION)
            {
                anyhow::bail!(
                    "a codec 74 mux connection takes mouse input only with a pane identity"
                );
            }
            InputSerial::empty()
        };
        if let Some(last) = mouse.queue.back_mut() {
            let mut merged = last.event;
            if last.scope.same_generation(&scope)
                && last.registration.same_registration(&registration)
                && last.source == source
                && coalesce_mouse_event(&mut merged, &event)
                && (source.is_none() || conditional_wheel_is_bounded(&merged))
            {
                last.event = merged;
                return Ok(());
            }
        }
        if mouse.queue.len() >= MOUSE_QUEUE_CAPACITY {
            metrics::counter!("mux.client.mouse_queue", "outcome" => "full").increment(1);
            anyhow::bail!("mouse input queue reached its {MOUSE_QUEUE_CAPACITY}-event capacity");
        }
        if mouse.queue.capacity() < MOUSE_QUEUE_CAPACITY {
            let additional = MOUSE_QUEUE_CAPACITY - mouse.queue.len();
            mouse.queue.try_reserve_exact(additional)?;
        }
        let retained_bytes = mouse
            .queue
            .capacity()
            .checked_mul(std::mem::size_of::<QueuedMouseEvent>())
            .and_then(|bytes| bytes.checked_add(4096))
            .ok_or_else(|| anyhow::anyhow!("mouse input queue admission size overflows"))?;
        let reservation = if mouse.worker_running {
            None
        } else {
            match promise::spawn::try_reserve_main_thread(
                promise::spawn::MainThreadServiceClass::Input,
                retained_bytes,
            ) {
                promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                    Some(reservation)
                }
                rejected => {
                    metrics::counter!("mux.client.mouse_queue", "outcome" => "scheduler_rejected")
                        .increment(1);
                    anyhow::bail!("main-thread scheduler rejected mouse input before queue admission: {rejected:?}");
                }
            }
        };
        mouse.queue.push_back(QueuedMouseEvent {
            event,
            scope,
            registration,
            source,
            input_serial,
        });
        mouse.worker_running = true;
        drop(mouse);
        if let Some(reservation) = reservation {
            let guard = MouseWorkerGuard {
                state: Arc::clone(state),
                drained: false,
            };
            reservation
                .spawn_local(async move {
                    Self::run(guard).await;
                })
                .detach();
        }
        Ok(())
    }

    async fn run(mut guard: MouseWorkerGuard) {
        let mut failure_notified = false;
        loop {
            let (entry, remote_pane_id) = {
                let mut mouse = guard.state.lock();
                let Some(entry) = mouse.queue.pop_front() else {
                    // Linearize drain with enqueue so no accepted event can
                    // be stranded between the old worker and its successor.
                    mouse.worker_running = false;
                    guard.drained = true;
                    return;
                };
                (entry, mouse.remote_pane_id)
            };
            let QueuedMouseEvent {
                event,
                scope,
                registration,
                source,
                input_serial,
            } = entry;
            let result = if let Some(source) = source {
                // Everything here was frozen when the input callback queued
                // the event. A later topology read cannot reauthorize it.
                let request = SendPaneInputConditionalV1 {
                    pane_id: remote_pane_id,
                    session_incarnation: source.session_incarnation,
                    pane_registration: source.pane_registration,
                    input_serial,
                    effect: PaneInputEffectV1::Mouse(event),
                };
                let expected = SendPaneInputConditionalV1Response {
                    pane_id: remote_pane_id,
                    session_incarnation: source.session_incarnation,
                    pane_registration: source.pane_registration,
                    input_serial,
                    outcome: PaneInputConditionalOutcomeV1::Accepted,
                };
                let admitted = registration.try_with_current(|_| {
                    let request = scope.send_pane_input_conditional_v1(request);
                    admit_interactive_rpc_now(async move {
                        validate_conditional_input_receipt(expected, request.await?)
                    })
                });
                let mut delivery =
                    ConditionalInputDelivery::new(registration.clone(), !failure_notified);
                let result = match admitted {
                    None => Err(anyhow::anyhow!(
                        "mouse input pane registration retired before dispatch"
                    )),
                    Some(Err(error)) => Err(error),
                    Some(Ok(None)) => Ok(()),
                    Some(Ok(Some(request))) => {
                        with_rpc_deadline(
                            Duration::from_secs(30),
                            "conditional mouse receipt",
                            request,
                        )
                        .await
                    }
                };
                delivery.settle(result.is_ok());
                failure_notified |= result.is_err();
                result
            } else {
                let admitted = registration.try_with_current(|_| {
                    admit_interactive_rpc_now(scope.mouse_event(SendMouseEvent {
                        pane_id: remote_pane_id,
                        event,
                    }))
                });
                match admitted {
                    None => Err(anyhow::anyhow!(
                        "mouse input pane registration retired before dispatch"
                    )),
                    Some(Err(error)) => Err(error),
                    Some(Ok(None)) => Ok(()),
                    Some(Ok(Some(request))) => request.await.map(|_| ()),
                }
            };
            if let Err(error) = result {
                // Mouse RPCs are not idempotent: never replay an ambiguous
                // effect. Keep subsequent releases ordered and report failure.
                metrics::counter!("mux.client.mouse_queue", "outcome" => "delivery_failed")
                    .increment(1);
                log::error!("remote mouse input delivery failed: {error:#}");
            }
            // Bound work per poll even when RPCs reject immediately.
            let mut yielded = false;
            futures::future::poll_fn(|context| {
                if std::mem::replace(&mut yielded, true) {
                    std::task::Poll::Ready(())
                } else {
                    context.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
        }
    }
}

fn conditional_wheel_is_bounded(event: &MouseEvent) -> bool {
    match event.button {
        MouseButton::WheelUp(count)
        | MouseButton::WheelDown(count)
        | MouseButton::WheelLeft(count)
        | MouseButton::WheelRight(count) => {
            (1..=MAX_CONDITIONAL_PANE_INPUT_WHEEL_STEPS).contains(&count)
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wezterm_term::KeyModifiers;

    #[test]
    fn current_mouse_coalescing_never_exceeds_the_server_effect_bound() {
        let original = MouseEvent {
            kind: MouseEventKind::Press,
            button: MouseButton::WheelUp(MAX_CONDITIONAL_PANE_INPUT_WHEEL_STEPS),
            x: 3,
            y: 4,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(conditional_wheel_is_bounded(&original));
        let mut merged = original;
        assert!(coalesce_mouse_event(&mut merged, &original));
        assert!(!conditional_wheel_is_bounded(&merged));
        for button in [
            MouseButton::WheelUp(0),
            MouseButton::WheelDown(0),
            MouseButton::WheelLeft(MAX_CONDITIONAL_PANE_INPUT_WHEEL_STEPS + 1),
            MouseButton::WheelRight(usize::MAX),
        ] {
            assert!(!conditional_wheel_is_bounded(&MouseEvent {
                button,
                ..original
            }));
        }
    }

    #[test]
    fn mouse_coalescing_preserves_wheel_identity_and_overflow() {
        let original = MouseEvent {
            kind: MouseEventKind::Press,
            button: MouseButton::WheelUp(2),
            x: 3,
            y: 4,
            x_pixel_offset: 1,
            y_pixel_offset: 2,
            modifiers: KeyModifiers::NONE,
        };
        let mut merged = original;
        assert!(coalesce_mouse_event(&mut merged, &original));
        assert_eq!(merged.button, MouseButton::WheelUp(4));
        for changed in [
            MouseEvent {
                kind: MouseEventKind::Release,
                ..original
            },
            MouseEvent { x: 9, ..original },
            MouseEvent { y: 9, ..original },
            MouseEvent {
                x_pixel_offset: 9,
                ..original
            },
            MouseEvent {
                y_pixel_offset: 9,
                ..original
            },
            MouseEvent {
                modifiers: KeyModifiers::SHIFT,
                ..original
            },
            MouseEvent {
                button: MouseButton::WheelDown(2),
                ..original
            },
            MouseEvent {
                button: MouseButton::WheelUp(usize::MAX),
                ..original
            },
        ] {
            let mut retained = original;
            assert!(!coalesce_mouse_event(&mut retained, &changed));
            assert_eq!(retained, original);
        }
        let mut movement = MouseEvent {
            kind: MouseEventKind::Move,
            button: MouseButton::Left,
            ..original
        };
        let latest = MouseEvent {
            x: 11,
            y: 12,
            ..movement
        };
        assert!(coalesce_mouse_event(&mut movement, &latest));
        assert_eq!(movement, latest);
    }
}
