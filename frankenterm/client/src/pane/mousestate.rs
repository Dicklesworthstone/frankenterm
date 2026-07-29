use crate::client::{Client, RpcGenerationScope};
use codec::*;
use mux::pane::PaneId;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wezterm_term::{MouseButton, MouseEvent, MouseEventKind};

pub struct MouseState {
    pending: AtomicBool,
    queue: VecDeque<QueuedMouseEvent>,
    client: Client,
    remote_pane_id: PaneId,
}

struct QueuedMouseEvent {
    event: MouseEvent,
    scope: RpcGenerationScope,
}

impl MouseState {
    pub fn new(remote_pane_id: PaneId, client: Client) -> Self {
        Self {
            remote_pane_id,
            client,
            pending: AtomicBool::new(false),
            queue: VecDeque::new(),
        }
    }

    pub fn append(&mut self, event: MouseEvent) {
        let scope = self.client.rpc_scope();
        if !scope.is_available() {
            log::trace!("dropping mouse event while mux RPC transport is unavailable");
            return;
        }
        if let Some(last) = self.queue.back_mut() {
            if last.scope.same_generation(&scope) && last.event.modifiers == event.modifiers {
                if last.event.kind == MouseEventKind::Move
                    && event.kind == MouseEventKind::Move
                    && last.event.button == event.button
                {
                    // Collapse any interim moves and just buffer up
                    // the last of them
                    *last = QueuedMouseEvent { event, scope };
                    return;
                }

                // Similarly, for repeated wheel scrolls, add up the deltas
                // rather than swamping the queue
                match (&last.event.button, &event.button) {
                    (MouseButton::WheelUp(a), MouseButton::WheelUp(b)) => {
                        last.event.button = MouseButton::WheelUp(a + b);
                        return;
                    }
                    (MouseButton::WheelDown(a), MouseButton::WheelDown(b)) => {
                        last.event.button = MouseButton::WheelDown(a + b);
                        return;
                    }
                    (MouseButton::WheelLeft(a), MouseButton::WheelLeft(b)) => {
                        last.event.button = MouseButton::WheelLeft(a + b);
                        return;
                    }
                    (MouseButton::WheelRight(a), MouseButton::WheelRight(b)) => {
                        last.event.button = MouseButton::WheelRight(a + b);
                        return;
                    }
                    _ => {}
                }
            }
        }
        self.queue.push_back(QueuedMouseEvent { event, scope });
        log::trace!("MouseEvent {}: queued", self.queue.len());
    }

    fn pop(&mut self) -> Option<QueuedMouseEvent> {
        if !self.pending.load(Ordering::SeqCst) {
            self.queue.pop_front()
        } else {
            None
        }
    }

    pub fn next(state: Arc<Mutex<Self>>) -> bool {
        let mut mouse = state.lock();
        if let Some(QueuedMouseEvent { event, scope }) = mouse.pop() {
            let state = Arc::clone(&state);
            mouse.pending.store(true, Ordering::SeqCst);
            let remote_pane_id = mouse.remote_pane_id;
            let request = scope.mouse_event(SendMouseEvent {
                pane_id: remote_pane_id,
                event,
            });

            promise::spawn::spawn(async move {
                request.await.ok();

                let mouse = state.lock();
                mouse.pending.store(false, Ordering::SeqCst);
                drop(mouse);

                Self::next(Arc::clone(&state));
                Ok::<(), anyhow::Error>(())
            })
            .detach();
            true
        } else {
            false
        }
    }
}
