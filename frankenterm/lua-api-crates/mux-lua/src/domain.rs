use super::*;
use mlua::UserDataRef;
use mux::domain::{DomainId, DomainState};
use mux::DomainOperationGuard;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainLifecycleEvent {
    /// Persist attachment authority before attempting a transport.
    Attached,
    /// Persist detachment authority before mutating the live domain.
    Detached,
    /// Rebuild retry supervision after an authorized attach attempt failed.
    AttachFailed,
    /// Fence and rebuild retry supervision after detachment was persisted.
    DetachedPersisted,
}

pub type DomainLifecycleFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
pub type DomainLifecycleRecorder = Arc<
    dyn Fn(String, DomainLifecycleEvent, DomainLifecycleWorkerHold) -> DomainLifecycleFuture
        + Send
        + Sync
        + 'static,
>;

static DOMAIN_LIFECYCLE_RECORDER: OnceLock<DomainLifecycleRecorder> = OnceLock::new();

struct DomainLifecycleLane {
    next_ticket: u64,
    active_ticket: Option<u64>,
    waiters: VecDeque<DomainLifecycleWaiter>,
}

struct DomainLifecycleWaiter {
    ticket: u64,
    ready: futures::channel::oneshot::Sender<()>,
}

static DOMAIN_LIFECYCLE_LANES: LazyLock<Mutex<BTreeMap<String, DomainLifecycleLane>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Synchronous admission ticket for one process-local domain lifecycle action.
///
/// Callers reserve before constructing fallible persistence or transport work,
/// then await [`Self::enter`]. Dropping either this reservation or the entered
/// guard advances the exact per-domain queue, so cancellation cannot strand a
/// successor. Different domain names remain independent.
#[must_use = "a lifecycle reservation must be entered or dropped to release its successor"]
pub struct DomainLifecycleReservation {
    domain_name: String,
    ticket: u64,
    ready: Option<futures::channel::oneshot::Receiver<()>>,
    release_required: bool,
}

/// Entered exact per-domain lifecycle authority.
#[must_use = "the lifecycle guard must span persistence, mutation, and handoff"]
pub struct DomainLifecycleGuard {
    entry: Arc<EnteredDomainLifecycle>,
}

/// A cancellation-safe hold transferred to an uncancellable persistence
/// worker. The per-domain lane advances only after both the foreground guard
/// and every admitted worker hold have been dropped.
#[must_use = "a lifecycle worker hold must remain owned by its admitted worker"]
pub struct DomainLifecycleWorkerHold {
    _entry: Arc<EnteredDomainLifecycle>,
}

struct EnteredDomainLifecycle {
    domain_name: String,
    ticket: u64,
    release_required: bool,
}

fn finish_domain_lifecycle(
    domain_name: &str,
    ticket: u64,
    release_required: &mut bool,
) {
    if !std::mem::take(release_required) {
        return;
    }

    // A waiter may have dropped its receiver before its reservation's Drop
    // implementation can acquire this mutex. Advance past those cancelled
    // tickets without ever making a later ticket concurrent with the active
    // predecessor.
    let mut released_ticket = ticket;
    loop {
        let next_waiter = {
            let mut lanes = DOMAIN_LIFECYCLE_LANES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(lane) = lanes.get_mut(domain_name) else {
                return;
            };

            let next_waiter = if lane.active_ticket == Some(released_ticket) {
                lane.active_ticket = None;
                let next = lane.waiters.pop_front();
                if let Some(next) = &next {
                    lane.active_ticket = Some(next.ticket);
                }
                next
            } else if let Some(position) = lane
                .waiters
                .iter()
                .position(|waiter| waiter.ticket == released_ticket)
            {
                lane.waiters.remove(position);
                None
            } else {
                return;
            };

            if lane.active_ticket.is_none() && lane.waiters.is_empty() {
                lanes.remove(domain_name);
            }
            next_waiter
        };

        let Some(next_waiter) = next_waiter else {
            return;
        };
        released_ticket = next_waiter.ticket;
        if next_waiter.ready.send(()).is_ok() {
            return;
        }
    }
}

impl DomainLifecycleReservation {
    pub async fn enter(mut self) -> anyhow::Result<DomainLifecycleGuard> {
        let ready = self
            .ready
            .take()
            .ok_or_else(|| anyhow::anyhow!("domain lifecycle readiness receiver is absent"))?;
        ready.await.map_err(|_| {
            anyhow::anyhow!("domain lifecycle readiness authority was lost before entry")
        })?;
        let guard = DomainLifecycleGuard {
            entry: Arc::new(EnteredDomainLifecycle {
                domain_name: self.domain_name.clone(),
                ticket: self.ticket,
                release_required: self.release_required,
            }),
        };
        self.release_required = false;
        Ok(guard)
    }
}

impl Drop for DomainLifecycleReservation {
    fn drop(&mut self) {
        finish_domain_lifecycle(
            &self.domain_name,
            self.ticket,
            &mut self.release_required,
        );
    }
}

impl Drop for EnteredDomainLifecycle {
    fn drop(&mut self) {
        finish_domain_lifecycle(
            &self.domain_name,
            self.ticket,
            &mut self.release_required,
        );
    }
}

impl DomainLifecycleGuard {
    /// Retain this exact lifecycle ticket across an uncancellable worker.
    #[must_use]
    pub fn worker_hold(&self) -> DomainLifecycleWorkerHold {
        DomainLifecycleWorkerHold {
            _entry: Arc::clone(&self.entry),
        }
    }
}

/// Reserve the next exact process-local lifecycle position for `domain_name`.
pub fn reserve_domain_lifecycle(
    domain_name: String,
) -> anyhow::Result<DomainLifecycleReservation> {
    let (ready_sender, ready) = futures::channel::oneshot::channel();
    let mut lanes = DOMAIN_LIFECYCLE_LANES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lane = lanes
        .entry(domain_name.clone())
        .or_insert_with(|| DomainLifecycleLane {
            next_ticket: 1,
            active_ticket: None,
            waiters: VecDeque::new(),
        });
    let ticket = lane.next_ticket;
    lane.next_ticket = lane.next_ticket.checked_add(1).ok_or_else(|| {
        anyhow::anyhow!(
            "domain lifecycle ticket namespace exhausted for {domain_name:?}"
        )
    })?;
    let ready_sender = if lane.active_ticket.is_none() {
        lane.active_ticket = Some(ticket);
        Some(ready_sender)
    } else {
        lane.waiters.push_back(DomainLifecycleWaiter {
            ticket,
            ready: ready_sender,
        });
        None
    };
    drop(lanes);
    if let Some(ready_sender) = ready_sender {
        if ready_sender.send(()).is_err() {
            let mut release_required = true;
            finish_domain_lifecycle(&domain_name, ticket, &mut release_required);
            anyhow::bail!("initial domain lifecycle admission was cancelled");
        }
    }
    Ok(DomainLifecycleReservation {
        domain_name,
        ticket,
        ready: Some(ready),
        release_required: true,
    })
}

/// Install the process-owned persistence and reconnect-lifecycle callback used
/// by GUI clients.
///
/// Non-GUI consumers intentionally leave this unset and retain the original
/// in-memory attach/detach behavior. The first installed recorder remains the
/// authority for the process lifetime so a config reload cannot replace it.
pub fn install_domain_lifecycle_recorder(recorder: DomainLifecycleRecorder) {
    let _ = DOMAIN_LIFECYCLE_RECORDER.set(recorder);
}

async fn record_domain_lifecycle(
    domain_name: String,
    event: DomainLifecycleEvent,
    lifecycle: &DomainLifecycleGuard,
) -> anyhow::Result<()> {
    if let Some(recorder) = DOMAIN_LIFECYCLE_RECORDER.get() {
        recorder(domain_name, event, lifecycle.worker_hold()).await?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub struct MuxDomain(pub DomainId);

impl MuxDomain {
    pub fn resolve(&self, mux: &Arc<Mux>) -> mlua::Result<DomainOperationGuard> {
        mux.get_domain(self.0)
            .ok_or_else(|| mlua::Error::external(format!("domain id {} not found in mux", self.0)))
    }
}

impl UserData for MuxDomain {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, _: ()| {
            Ok(format!(
                "MuxDomain(domain_id:{}, pid:{})",
                this.0,
                std::process::id()
            ))
        });
        methods.add_method("domain_id", |_, this, _: ()| Ok(this.0));

        methods.add_method("is_spawnable", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            Ok(domain.spawnable())
        });

        // Must stay an async Lua method. `promise::spawn::block_on` (the form
        // this regressed to during the mlua 0.11 migration) trips the
        // main-thread dispatch guard -> SIGABRT when `domain:attach` is invoked
        // from a `gui-startup` handler / event handler / keybinding running on
        // the main-thread spawn queue.
        //
        // We can't simply `domain.attach(..).await` directly: `Domain::attach`
        // is an `#[async_trait(?Send)]` method, so its future is `!Send` (it
        // boxes a non-Send `dyn Future`, and for remote `ClientDomain`s drives
        // network RPCs), but mlua 0.11's `add_async_method` requires a `Send`
        // future. So spawn the `!Send` work onto the main-thread queue via
        // `promise::spawn::spawn` (which uses `spawn_local`, lifting the `Send`
        // bound on the future) and await the resulting `Task` handle, which IS
        // `Send`. The event loop drives the spawned future to completion while
        // we yield here; no `block_on`, so no dispatch-guard trip, and only the
        // `Send` `Task` is live across the await, satisfying mlua.
        methods.add_async_method(
            "attach",
            |_, this, window: Option<UserDataRef<MuxWindow>>| async move {
                let mux = get_mux()?;
                let domain = this.resolve(&mux)?;
                let domain_name = domain.domain_name().to_string();
                let window_id = window.map(|w| w.0);
                let owner_client_id = mux.active_identity();
                crate::run_on_main_thread(
                    promise::spawn::MainThreadServiceClass::Topology,
                    "attach domain",
                    || async move {
                        let lifecycle = reserve_domain_lifecycle(domain_name.clone())
                            .map_err(mlua::Error::external)?
                            .enter()
                            .await
                            .map_err(mlua::Error::external)?;
                        let detachable = domain.detachable();
                        let attach_result = domain.attach(&mux, owner_client_id, window_id).await;
                        if detachable {
                            record_domain_lifecycle(
                                domain_name.clone(),
                                DomainLifecycleEvent::Attached,
                                &lifecycle,
                            )
                            .await
                            .map_err(mlua::Error::external)?;
                        }
                        match attach_result {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                let attach_error = mlua::Error::external(format!(
                                    "failed to attach domain {}: {error:#}",
                                    domain.domain_name()
                                ));
                                if !detachable {
                                    return Err(attach_error);
                                }
                                match record_domain_lifecycle(
                                    domain_name,
                                    DomainLifecycleEvent::AttachFailed,
                                    &lifecycle,
                                )
                                .await
                                {
                                    Ok(()) => Err(attach_error),
                                    Err(retry_error) => Err(mlua::Error::external(format!(
                                        "{attach_error}; automatic retry could not be scheduled: {retry_error:#}"
                                    ))),
                                }
                            }
                        }
                    },
                )
                .await?
            },
        );

        methods.add_async_method("detach", |_, this, _: ()| async move {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            let domain_name = domain.domain_name().to_string();
            crate::run_on_main_thread(
                promise::spawn::MainThreadServiceClass::Topology,
                "detach domain",
                || async move {
                    let lifecycle = reserve_domain_lifecycle(domain_name.clone())
                        .map_err(mlua::Error::external)?
                        .enter()
                        .await
                        .map_err(mlua::Error::external)?;
                    if domain.detachable() {
                        record_domain_lifecycle(
                            domain_name.clone(),
                            DomainLifecycleEvent::Detached,
                            &lifecycle,
                        )
                        .await
                        .map_err(mlua::Error::external)?;
                        record_domain_lifecycle(
                            domain_name,
                            DomainLifecycleEvent::DetachedPersisted,
                            &lifecycle,
                        )
                        .await
                        .map_err(mlua::Error::external)?;
                    }
                    domain.detach().map_err(|err| {
                        mlua::Error::external(format!(
                            "failed to detach domain {}: {err:#}",
                            domain.domain_name()
                        ))
                    })
                },
            )
            .await?
        });

        methods.add_method("state", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            Ok(match domain.state() {
                DomainState::Attached => "Attached",
                DomainState::Detached => "Detached",
            })
        });

        methods.add_method("name", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            Ok(domain.domain_name().to_string())
        });

        // Async for the same reason as `attach`. `Domain::domain_label` is an
        // `#[async_trait(?Send)]` method (its future is `!Send`); block_on from
        // the main-thread spawn queue trips the GUI dispatch deadlock guard, and
        // mlua 0.11 requires a `Send` future. Spawn the `!Send` work onto the
        // main-thread queue and await the `Send` `Task` handle (see `attach`).
        methods.add_async_method("label", |_, this, _: ()| async move {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            crate::run_on_main_thread(
                promise::spawn::MainThreadServiceClass::Interactive,
                "read domain label",
                || async move { domain.domain_label().await },
            )
            .await
        });

        methods.add_method("has_any_panes", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            let have_panes_in_domain = mux
                .iter_panes()
                .iter()
                .any(|p| p.domain_id() == domain.domain_id());
            Ok(have_panes_in_domain)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_domain_lifecycle_is_ticket_ordered_and_cancel_safe() {
        let name = "sequencer-ticket-order".to_string();
        let first = futures::executor::block_on(
            reserve_domain_lifecycle(name.clone())
                .expect("reserve first lifecycle action")
                .enter(),
        )
        .expect("enter first lifecycle action");
        let second = reserve_domain_lifecycle(name.clone())
            .expect("reserve second lifecycle action");
        let mut second_enter = Box::pin(second.enter());
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(matches!(
            second_enter.as_mut().poll(&mut context),
            std::task::Poll::Pending
        ));
        drop(first);
        let second = match second_enter.as_mut().poll(&mut context) {
            std::task::Poll::Ready(Ok(second)) => second,
            std::task::Poll::Ready(Err(error)) => {
                panic!("second action lost lifecycle authority: {error:#}")
            }
            std::task::Poll::Pending => panic!("second action was not released in ticket order"),
        };
        drop(second);

        let cancelled = reserve_domain_lifecycle(name.clone())
            .expect("reserve cancellable lifecycle action");
        let successor = reserve_domain_lifecycle(name)
            .expect("reserve successor after cancellable action");
        drop(cancelled);
        drop(
            futures::executor::block_on(successor.enter())
                .expect("enter successor after cancellable action"),
        );
    }

    #[test]
    fn cancelling_queued_tail_cannot_let_a_successor_overtake_active_ticket() {
        let name = "sequencer-cancelled-tail".to_string();
        let active = futures::executor::block_on(
            reserve_domain_lifecycle(name.clone())
                .expect("reserve active lifecycle action")
                .enter(),
        )
        .expect("enter active lifecycle action");
        let cancelled_tail =
            reserve_domain_lifecycle(name.clone()).expect("reserve cancellable tail action");
        drop(cancelled_tail);

        let successor = reserve_domain_lifecycle(name).expect("reserve successor action");
        let mut successor_enter = Box::pin(successor.enter());
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(matches!(
            successor_enter.as_mut().poll(&mut context),
            std::task::Poll::Pending
        ));
        drop(active);
        let successor = match successor_enter.as_mut().poll(&mut context) {
            std::task::Poll::Ready(Ok(successor)) => successor,
            std::task::Poll::Ready(Err(error)) => {
                panic!("successor lost lifecycle authority: {error:#}")
            }
            std::task::Poll::Pending => {
                panic!("successor was not released after the active ticket completed")
            }
        };
        drop(successor);
    }

    #[test]
    fn distinct_domain_lifecycle_lanes_are_independent() {
        let first = futures::executor::block_on(
            reserve_domain_lifecycle("sequencer-domain-a".to_string())
                .expect("reserve domain A")
                .enter(),
        )
        .expect("enter domain A");
        let second = futures::executor::block_on(
            reserve_domain_lifecycle("sequencer-domain-b".to_string())
                .expect("reserve domain B")
                .enter(),
        )
        .expect("enter domain B");
        drop(second);
        drop(first);
    }

    #[test]
    fn worker_hold_prevents_a_cancelled_foreground_from_releasing_its_successor() {
        let name = "sequencer-worker-hold".to_string();
        let foreground = futures::executor::block_on(
            reserve_domain_lifecycle(name.clone())
                .expect("reserve foreground lifecycle action")
                .enter(),
        )
        .expect("enter foreground lifecycle action");
        let worker_hold = foreground.worker_hold();
        let successor = reserve_domain_lifecycle(name).expect("reserve successor action");
        let mut successor_enter = Box::pin(successor.enter());
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);

        drop(foreground);
        assert!(matches!(
            successor_enter.as_mut().poll(&mut context),
            std::task::Poll::Pending
        ));

        drop(worker_hold);
        let successor = match successor_enter.as_mut().poll(&mut context) {
            std::task::Poll::Ready(Ok(successor)) => successor,
            std::task::Poll::Ready(Err(error)) => {
                panic!("worker-held successor lost lifecycle authority: {error:#}")
            }
            std::task::Poll::Pending => {
                panic!("successor was not released after the worker hold completed")
            }
        };
        drop(successor);
    }

    #[test]
    fn lua_attach_and_detach_each_use_one_admitted_topology_transaction() {
        let source = include_str!("domain.rs");
        let attach_start = source
            .find("methods.add_async_method(\n            \"attach\"")
            .expect("Lua attach method remains present");
        let detach_start = source[attach_start..]
            .find("methods.add_async_method(\"detach\"")
            .map(|offset| attach_start + offset)
            .expect("Lua detach method remains present");
        let state_start = source[detach_start..]
            .find("methods.add_method(\"state\"")
            .map(|offset| detach_start + offset)
            .expect("Lua detach method remains bounded");
        let attach = &source[attach_start..detach_start];
        let detach = &source[detach_start..state_start];
        let transport = attach
            .find("let attach_result = domain.attach")
            .expect("explicit Lua attach starts its transport");
        let attached_intent = attach
            .find("DomainLifecycleEvent::Attached")
            .expect("explicit Lua attach persists requested intent");
        assert!(
            transport < attached_intent,
            "best-effort remembrance must not gate the explicit Lua transport"
        );
        assert_eq!(attach.matches("crate::run_on_main_thread(").count(), 1);
        assert_eq!(detach.matches("crate::run_on_main_thread(").count(), 1);
        for transaction in [attach, detach] {
            let admission = transaction
                .find("crate::run_on_main_thread(")
                .expect("transaction has scheduler admission");
            let lifecycle = transaction
                .find("reserve_domain_lifecycle")
                .expect("transaction has per-domain ordering");
            let persistence = transaction
                .find("record_domain_lifecycle")
                .expect("transaction has durable lifecycle persistence");
            assert!(
                admission < lifecycle && lifecycle < persistence,
                "scheduler admission and lifecycle ordering must precede persistence"
            );
        }
    }
}
