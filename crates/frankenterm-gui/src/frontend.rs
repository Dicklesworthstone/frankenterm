use crate::TermWindow;
use crate::scripting::guiwin::GuiWin;
use crate::spawn::SpawnWhere;
use crate::termwindow::TermWindowNotif;
use ::window::*;
use anyhow::{Context, Error};
use config::keyassignment::{KeyAssignment, SpawnCommand};
use config::{ConfigSubscription, NotificationHandling};
use frankenterm_core::osc_protocol_integration::{CursorShapeSlug, Osc22PerPaneCursorMap};
use frankenterm_gui::workspace_reconcile::{WorkspaceReconcileGate, WorkspaceReconcileWaiters};
use frankenterm_toast_notification::*;
use mux::client::ClientId;
use mux::window::WindowId as MuxWindowId;
use mux::{Mux, MuxNotification};
use promise::spawn::{
    MainThreadReservationOutcome, MainThreadServiceClass, try_reserve_main_thread,
};
use promise::{Future, Promise};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use wezterm_term::{Alert, ClipboardSelection};

const MAX_RECONCILE_WAITERS: usize = 4_096;
const FRONTEND_MAIN_THREAD_ESTIMATED_BYTES: usize = 4 * 1024;
static LAYOUT_PENDING: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Terminal delivery is fenced only during the short queued restore cut.
/// UI bindings and selection/copy do not call this gate.
pub(crate) fn layout_input_ready(window_id: MuxWindowId, pane: &Arc<dyn mux::pane::Pane>) -> bool {
    if LAYOUT_PENDING.load(std::sync::atomic::Ordering::Acquire) & 2 != 0 {
        return false;
    }
    let Some(mux) = Mux::try_get() else {
        return false;
    };
    if !layout_pane_belongs_to_window(&mux, window_id, pane) {
        return false;
    }
    if let Some(domain) = mux.get_domain(pane.domain_id()) {
        if let Some(client) = domain.downcast_ref::<ClientDomain>() {
            return !client.layout_restore_pending();
        }
    }
    true
}

pub(crate) fn layout_pane_belongs_to_window(
    mux: &Mux,
    window_id: MuxWindowId,
    pane: &Arc<dyn mux::pane::Pane>,
) -> bool {
    mux.get_pane(pane.pane_id())
        .is_some_and(|current| Arc::ptr_eq(&current, pane))
        && mux
            .resolve_pane_id(pane.pane_id())
            .is_some_and(|(_, owner, _)| owner == window_id)
}

use crate::window_state_persist::{
    LayoutStateSnapshot, LayoutWindowId, MixedDomainLayoutOverlay, StableLocalSessionId,
    StableLocalTabId, StableMuxSessionId, StableTabSlot,
};
use frankenterm_client::domain::{ClientDomain, RemoteLayoutSnapshot};

struct OwnedLayoutWindow {
    mux_identity: uuid::Uuid,
    overlay: MixedDomainLayoutOverlay,
}

struct LayoutLifecycle {
    startup: LayoutStateSnapshot,
    owned: HashMap<MuxWindowId, OwnedLayoutWindow>,
    restored: std::collections::BTreeSet<LayoutWindowId>,
}

struct LiveLayout {
    receipts: Vec<Arc<RemoteLayoutSnapshot>>,
    slots: HashMap<mux::tab::TabId, StableTabSlot>,
    unavailable: std::collections::BTreeSet<crate::window_state_persist::DomainBindingId>,
}

impl LiveLayout {
    fn capture(mux: &Arc<Mux>, startup: &LayoutStateSnapshot) -> anyhow::Result<Self> {
        let mut result = Self {
            receipts: Vec::new(),
            slots: HashMap::new(),
            unavailable: startup
                .domain_bindings
                .iter()
                .map(|binding| binding.binding_id())
                .collect(),
        };
        for domain in mux.iter_domains() {
            let Some(client) = domain.downcast_ref::<ClientDomain>() else {
                continue;
            };
            let Some(receipt) = client.layout_snapshot() else {
                continue;
            };
            receipt.with_current(mux, || {
                let binding = crate::window_state_persist::DomainBindingId::from_bytes(
                    receipt.binding().binding_id.as_bytes(),
                );
                result.unavailable.remove(&binding);
                for entry in receipt.tabs() {
                    anyhow::ensure!(result.slots.len() < 16_384, "live layout exceeds tab bound");
                    let slot = StableTabSlot::remote(
                        binding,
                        StableMuxSessionId::from_bytes(receipt.session().as_bytes()),
                        u64::try_from(entry.remote_window_id())?,
                        u64::try_from(entry.remote_tab_id())?,
                    );
                    anyhow::ensure!(
                        result.slots.insert(entry.tab().tab_id(), slot).is_none(),
                        "multiple attachments claim one local layout tab"
                    );
                }
                Ok(())
            })?;
            result.receipts.push(receipt);
        }
        let (session, _) = mux.topology_snapshot_authority()?;
        for window in mux.iter_windows_bounded(4_096)? {
            let Some(order) = mux.window_order_snapshot(window)? else {
                continue;
            };
            for tab in order.ordered_tabs() {
                if result.slots.contains_key(&tab.tab_id()) {
                    continue;
                }
                // A client tab without a current receipt is unavailable, not a
                // local tab. In particular codec46 numeric IDs cannot persist.
                if tab.iter_panes().iter().any(|pane| {
                    mux.get_domain(pane.pane.domain_id())
                        .is_none_or(|domain| domain.downcast_ref::<ClientDomain>().is_some())
                }) {
                    continue;
                }
                anyhow::ensure!(result.slots.len() < 16_384, "live layout exceeds tab bound");
                result.slots.insert(
                    tab.tab_id(),
                    StableTabSlot::local(
                        StableLocalSessionId::from_bytes(session.as_bytes()),
                        StableLocalTabId::from_bytes(*tab.durable_id().as_bytes()),
                    ),
                );
            }
        }
        Ok(result)
    }

    fn with_current<T>(
        &self,
        mux: &Arc<Mux>,
        apply: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        RemoteLayoutSnapshot::with_current_batch(&self.receipts, mux, apply)
    }
}

impl LayoutLifecycle {
    fn capture_changes(&mut self, mux: &Arc<Mux>, live: &LiveLayout) -> anyhow::Result<()> {
        let mut updates = Vec::new();
        let mut next_owned = Vec::new();
        for id in mux.iter_windows_bounded(4_096)? {
            let Some(order) = mux.window_order_snapshot(id)? else {
                continue;
            };
            let Some(window) = mux.get_window(id) else {
                continue;
            };
            let identity = window.durable_id();
            let workspace = window.get_workspace().to_owned();
            drop(window);
            let Some(slots): Option<Vec<_>> = order
                .ordered_tabs()
                .iter()
                .map(|tab| live.slots.get(&tab.tab_id()).copied())
                .collect()
            else {
                continue;
            };
            let old = self
                .owned
                .get(&id)
                .filter(|owned| owned.mux_identity == identity);
            if old.is_none()
                && self.startup.overlays.iter().any(|overlay| {
                    !self.restored.contains(&overlay.window_id())
                        && overlay.slots().iter().any(|saved| {
                            slots.iter().any(|slot| slot.identity() == saved.identity())
                        })
                })
            {
                // Startup owns this association. A queued capture must not
                // persist a competing freshly minted ID before restore runs.
                continue;
            }
            // Pure remote order belongs to its server. This store describes
            // local/mixed composition only, including retained unavailable slots.
            let pure_remote = slots.first().is_some_and(|first| match first {
                StableTabSlot::Remote { binding_id, session_id, remote_window_id, .. } => slots.iter().all(|slot|
                    matches!(slot, StableTabSlot::Remote { binding_id: b, session_id: s, remote_window_id: w, .. }
                        if b == binding_id && s == session_id && w == remote_window_id)),
                _ => false,
            });
            if old.is_none() && (pure_remote || slots.is_empty()) {
                continue;
            }
            let active = order
                .active_tab_id()
                .and_then(|id| live.slots.get(&id).copied());
            let mut slots = slots;
            if let Some(old) = old {
                for (position, slot) in old.overlay.slots().iter().enumerate() {
                    if slot
                        .remote_binding()
                        .is_some_and(|binding| live.unavailable.contains(&binding))
                        && !slots.iter().any(|live| live.identity() == slot.identity())
                    {
                        slots.insert(position.min(slots.len()), *slot);
                    }
                }
            }
            if old.is_some_and(|old| {
                old.overlay.slots() == slots.as_slice()
                    && old.overlay.active() == active
                    && old.overlay.workspace() == workspace
            }) {
                continue;
            }
            let (window_id, revision, base) = match old {
                Some(old) => (
                    old.overlay.window_id(),
                    old.overlay
                        .local_revision()
                        .checked_add(1)
                        .context("layout revision exhausted")?,
                    Some(old.overlay.local_revision()),
                ),
                None => (self.startup.new_layout_window_id()?, 1, None),
            };
            let overlay =
                MixedDomainLayoutOverlay::new(window_id, workspace, revision, slots, active)?;
            updates.push((base, overlay.clone()));
            next_owned.push((
                id,
                OwnedLayoutWindow {
                    mux_identity: identity,
                    overlay,
                },
            ));
        }
        if updates.is_empty() {
            return Ok(());
        }
        live.with_current(mux, || {
            crate::window_state_persist::queue_layout_overlays(updates).map_err(Into::into)
        })?;
        for (id, owned) in next_owned {
            // A local edit supersedes the startup placement, including edits
            // made while another domain is unavailable.
            if let Some(startup) = self
                .startup
                .overlays
                .iter_mut()
                .find(|overlay| overlay.window_id() == owned.overlay.window_id())
            {
                *startup = owned.overlay.clone();
            }
            self.owned.insert(id, owned);
        }
        Ok(())
    }

    fn restore(&mut self, mux: &Arc<Mux>, live: &LiveLayout) -> anyhow::Result<()> {
        let mut orders = BTreeMap::new();
        for id in mux.iter_windows_bounded(4_096)? {
            if let Some(order) = mux.window_order_snapshot(id)? {
                orders.insert(id, order);
            }
        }
        let mut by_identity = HashMap::new();
        for order in orders.values() {
            for tab in order.ordered_tabs() {
                if let Some(slot) = live.slots.get(&tab.tab_id()) {
                    anyhow::ensure!(
                        by_identity
                            .insert(slot.identity(), Arc::clone(tab))
                            .is_none(),
                        "live layout aliases a stable tab identity"
                    );
                }
            }
        }
        let mut desired: BTreeMap<_, _> = orders
            .iter()
            .map(|(&id, order)| {
                (
                    id,
                    (order.ordered_tabs().to_vec(), order.active_tab().cloned()),
                )
            })
            .collect();
        let mut claimed_windows = std::collections::BTreeSet::new();
        let mut assignments = Vec::new();
        let mut builders = Vec::new();
        let result = (|| -> anyhow::Result<()> {
            for overlay in &self.startup.overlays {
                if self.restored.contains(&overlay.window_id()) {
                    continue;
                }
                let tabs: Vec<_> = overlay
                    .slots()
                    .iter()
                    .filter_map(|slot| by_identity.get(&slot.identity()).cloned())
                    .collect();
                let Some(first) = tabs.first() else {
                    continue;
                };
                let existing = self.owned.iter().find_map(|(&id, owned)| {
                    (owned.overlay.window_id() == overlay.window_id()
                        && mux
                            .get_window(id)
                            .is_some_and(|window| window.durable_id() == owned.mux_identity))
                    .then_some(id)
                });
                let source = orders
                    .iter()
                    .find_map(|(&id, order)| {
                        order
                            .ordered_tabs()
                            .iter()
                            .any(|tab| Arc::ptr_eq(tab, first))
                            .then_some(id)
                    })
                    .context("restored tab lost its parent")?;
                let mut target = existing.unwrap_or(source);
                let compatible_workspace = mux
                    .get_window(target)
                    .is_some_and(|window| window.get_workspace() == overlay.workspace());
                let other_owner = self
                    .owned
                    .get(&target)
                    .is_some_and(|owned| owned.overlay.window_id() != overlay.window_id());
                if !compatible_workspace || other_owner || !claimed_windows.insert(target) {
                    let builder = mux.new_empty_window(Some(overlay.workspace().to_owned()), None);
                    target = *builder;
                    let order = mux
                        .window_order_snapshot(target)?
                        .context("new layout window disappeared")?;
                    orders.insert(target, order);
                    desired.insert(target, (Vec::new(), None));
                    builders.push(builder);
                    claimed_windows.insert(target);
                }
                let selected: std::collections::HashSet<_> =
                    tabs.iter().map(|tab| tab.tab_id()).collect();
                for (ordered, active) in desired.values_mut() {
                    ordered.retain(|tab| !selected.contains(&tab.tab_id()));
                    if active
                        .as_ref()
                        .is_some_and(|tab| selected.contains(&tab.tab_id()))
                    {
                        *active = ordered.first().cloned();
                    }
                }
                let (ordered, active) = desired
                    .get_mut(&target)
                    .context("layout target disappeared")?;
                let mut next = tabs;
                next.append(ordered);
                *active = overlay
                    .active()
                    .and_then(|slot| by_identity.get(&slot.identity()).cloned())
                    .filter(|tab| next.iter().any(|candidate| Arc::ptr_eq(candidate, tab)))
                    .or_else(|| active.clone())
                    .or_else(|| next.first().cloned());
                *ordered = next;
                assignments.push((target, overlay.clone()));
            }
            if assignments.is_empty() {
                return Ok(());
            }
            let mirrors = orders
                .into_iter()
                .map(|(id, expected)| {
                    let (ordered_tabs, active_tab) = desired
                        .remove(&id)
                        .expect("every frozen window has a desired state");
                    mux::WindowOrderMirror {
                        expected,
                        ordered_tabs,
                        active_tab,
                    }
                })
                .collect();
            live.with_current(mux, || mux.apply_window_order_mirrors(mirrors).map(|_| ()))?;
            for (id, overlay) in assignments {
                let identity = mux
                    .get_window(id)
                    .context("committed layout window disappeared")?
                    .durable_id();
                if !overlay.slots().iter().any(|slot| {
                    slot.remote_binding()
                        .is_some_and(|binding| live.unavailable.contains(&binding))
                }) {
                    self.restored.insert(overlay.window_id());
                }
                self.owned.insert(
                    id,
                    OwnedLayoutWindow {
                        mux_identity: identity,
                        overlay,
                    },
                );
            }
            Ok(())
        })();
        // Empty provisional windows cancel without publication; successfully
        // populated windows retain their exact sessions on every exit path.
        for builder in builders {
            builder.cancel();
        }
        result
    }
}

fn topology_needs_workspace_reconcile(change: &mux::FrozenWindowTopologyChange) -> bool {
    !change.created_windows().is_empty()
        || !change.removed_windows().is_empty()
        || !change.attached_tabs().is_empty()
}

fn layout_attachment_ready(_: mux::domain::DomainId) {
    schedule_layout_reconcile(true);
}

struct LayoutReconcileAdmission<'a> {
    pending: &'a std::sync::atomic::AtomicU8,
    armed: bool,
}

impl LayoutReconcileAdmission<'_> {
    fn begin(mut self) -> u8 {
        self.armed = false;
        self.pending.swap(0, std::sync::atomic::Ordering::AcqRel)
    }
}

impl Drop for LayoutReconcileAdmission<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.pending.store(0, std::sync::atomic::Ordering::Release);
            log::warn!(
                "layout reconciliation was cancelled before execution; next topology signal may retry"
            );
        }
    }
}

fn schedule_layout_reconcile(restore: bool) {
    use std::sync::atomic::Ordering;
    let flag = if restore { 2 } else { 1 };
    if LAYOUT_PENDING.fetch_or(flag, Ordering::AcqRel) != 0 {
        return;
    }
    let admission = LayoutReconcileAdmission {
        pending: &LAYOUT_PENDING,
        armed: true,
    };
    match try_reserve_main_thread(
        MainThreadServiceClass::Topology,
        FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
    ) {
        MainThreadReservationOutcome::Reserved(reservation) => {
            reservation
                .handoff_to_main_thread_local(move |reservation| {
                    reservation
                        .spawn_local(async move {
                            let pending = admission.begin();
                            if let Some(frontend) = try_front_end() {
                                if pending & 2 != 0 {
                                    frontend.reconcile_layout(true);
                                }
                                if pending & 1 != 0 {
                                    frontend.reconcile_layout(false);
                                }
                                if pending & 2 != 0 {
                                    frontend.reconcile_workspace();
                                }
                            }
                        })
                        .detach();
                })
                .detach();
        }
        rejected => {
            drop(admission);
            log::warn!("layout reconciliation admission refused: {rejected:?}");
        }
    }
}

/// Close only the newly allocated native view if initialization does not hand
/// it to the frontend. The cleanup captures the exact platform window handle.
#[cfg(any(test, not(target_os = "macos")))]
pub(crate) struct PendingNativeWindow<F: FnOnce()> {
    close: Option<F>,
}

#[cfg(any(test, not(target_os = "macos")))]
impl<F: FnOnce()> PendingNativeWindow<F> {
    pub(crate) fn new(close: F) -> Self {
        Self { close: Some(close) }
    }

    pub(crate) fn publish(mut self) {
        drop(self.close.take());
    }
}

#[cfg(any(test, not(target_os = "macos")))]
impl<F: FnOnce()> Drop for PendingNativeWindow<F> {
    fn drop(&mut self) {
        if let Some(close) = self.close.take() {
            close();
        }
    }
}

/// Owns only a pending native view, never the sessions behind that view.
/// Cancellation and errors release exactly this attempt's registration.
struct PendingWindowCreation {
    pending: Rc<RefCell<HashMap<MuxWindowId, Rc<()>>>>,
    window_id: MuxWindowId,
    identity: Rc<()>,
}

impl PendingWindowCreation {
    fn try_begin(
        pending: &Rc<RefCell<HashMap<MuxWindowId, Rc<()>>>>,
        window_id: MuxWindowId,
    ) -> Option<Self> {
        let mut entries = pending.borrow_mut();
        let std::collections::hash_map::Entry::Vacant(entry) = entries.entry(window_id) else {
            return None;
        };
        let identity = Rc::new(());
        entry.insert(Rc::clone(&identity));
        Some(Self {
            pending: Rc::clone(pending),
            window_id,
            identity,
        })
    }
}

impl Drop for PendingWindowCreation {
    fn drop(&mut self) {
        let mut entries = self.pending.borrow_mut();
        if entries
            .get(&self.window_id)
            .is_some_and(|current| Rc::ptr_eq(current, &self.identity))
        {
            entries.remove(&self.window_id);
        }
    }
}

/// Releases the reconciliation gate even if its detached task is never polled
/// or is cancelled while native window creation is awaiting completion.
struct PendingWorkspaceReconcile {
    identity: Rc<()>,
    current: Rc<RefCell<Option<Rc<()>>>>,
    gate: Rc<Cell<WorkspaceReconcileGate>>,
    waiters: Rc<RefCell<WorkspaceReconcileWaiters>>,
}

impl PendingWorkspaceReconcile {
    fn new(
        current: &Rc<RefCell<Option<Rc<()>>>>,
        gate: &Rc<Cell<WorkspaceReconcileGate>>,
        waiters: &Rc<RefCell<WorkspaceReconcileWaiters>>,
    ) -> Self {
        let identity = Rc::new(());
        *current.borrow_mut() = Some(Rc::clone(&identity));
        Self {
            identity,
            current: Rc::clone(current),
            gate: Rc::clone(gate),
            waiters: Rc::clone(waiters),
        }
    }

    fn release_identity(&self) -> bool {
        let mut current = self.current.borrow_mut();
        if current
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, &self.identity))
        {
            current.take();
            true
        } else {
            false
        }
    }

    fn finish(self, failure: Option<&'static str>) -> Option<bool> {
        if !self.release_identity() {
            return None;
        }
        let mut gate = self.gate.get();
        let run_again = gate.finish_pass();
        self.gate.set(gate);
        let completed = self.waiters.borrow_mut().finish_active_pass(run_again);
        for mut promise in completed {
            match failure {
                Some(reason) => {
                    promise.err(Error::msg(reason));
                }
                None => {
                    promise.ok(());
                }
            }
        }
        Some(run_again)
    }
}

impl Drop for PendingWorkspaceReconcile {
    fn drop(&mut self) {
        if !self.release_identity() {
            return;
        }
        let mut gate = self.gate.get();
        gate.cancel_pass();
        self.gate.set(gate);
        let cancelled = self.waiters.borrow_mut().cancel_all();
        // Wake callers only after every shared borrow is released. A caller
        // can immediately retry, creating a new independently owned pass.
        for mut promise in cancelled {
            promise.err(Error::msg("workspace reconciliation was cancelled"));
        }
        metrics::counter!("gui.workspace_reconcile_cancelled.total").increment(1);
    }
}

fn schedule_frontend_main_thread<MAKE, FUT>(
    service_class: MainThreadServiceClass,
    estimated_bytes: usize,
    operation: &'static str,
    make_future: MAKE,
) where
    MAKE: FnOnce() -> FUT + Send + 'static,
    FUT: std::future::Future<Output = ()> + 'static,
{
    match try_reserve_main_thread(service_class, estimated_bytes) {
        MainThreadReservationOutcome::Reserved(reservation) => {
            // Config reloads and mux notifications can originate on worker
            // threads. Construct the local future only after transferring this
            // exact admission to the GUI thread, where it will also be polled.
            reservation
                .handoff_to_main_thread_local(move |reservation| {
                    reservation.spawn_local(make_future()).detach();
                })
                .detach();
        }
        rejected => {
            metrics::counter!(
                "gui.main_thread_admission",
                "operation" => operation,
                "outcome" => "terminal_rejection"
            )
            .increment(1);
            log::error!(
                "GUI main-thread scheduler rejected {operation} before task construction: {rejected:?}"
            );
        }
    }
}

pub struct GuiFrontEnd {
    connection: Rc<Connection>,
    switching_workspaces: RefCell<bool>,
    spawned_mux_window: Rc<RefCell<HashMap<MuxWindowId, Rc<()>>>>,
    known_windows: RefCell<BTreeMap<Window, MuxWindowId>>,
    osc22_cursor_shapes: RefCell<Osc22PerPaneCursorMap>,
    client_id: Arc<ClientId>,
    config_subscription: RefCell<Option<ConfigSubscription>>,
    workspace_reconcile_gate: Rc<Cell<WorkspaceReconcileGate>>,
    workspace_reconcile_waiters: Rc<RefCell<WorkspaceReconcileWaiters>>,
    workspace_reconcile_pass: Rc<RefCell<Option<Rc<()>>>>,
    osc52_dispatch_identity: Arc<()>,
    layout_lifecycle: RefCell<Option<LayoutLifecycle>>,
    applying_layout: Cell<bool>,
}

impl Drop for GuiFrontEnd {
    fn drop(&mut self) {
        ::window::shutdown();
    }
}

impl GuiFrontEnd {
    fn reconcile_layout(&self, restore: bool) {
        if self.applying_layout.replace(true) {
            return;
        }
        let result = (|| -> anyhow::Result<()> {
            let mux = Mux::try_get().context("layout mux is unavailable")?;
            let mut lifecycle = self.layout_lifecycle.borrow_mut();
            let Some(lifecycle) = lifecycle.as_mut() else {
                return Ok(());
            };
            let live = LiveLayout::capture(&mux, &lifecycle.startup)?;
            if restore {
                lifecycle.restore(&mux, &live)
            } else {
                lifecycle.capture_changes(&mux, &live)
            }
        })();
        self.applying_layout.set(false);
        if let Err(error) = result {
            log::warn!(
                "mixed layout reconciliation unavailable; preserving live sessions: {error:#}"
            );
        }
    }

    pub fn try_new() -> anyhow::Result<Rc<GuiFrontEnd>> {
        let connection = Connection::init()?;
        connection.set_event_handler(Self::app_event_handler);

        let mux = Mux::try_get().context("mux singleton is not available")?;
        let client_id = mux
            .active_identity()
            .context("active mux identity is not set")?;
        let mux_owner = Arc::downgrade(&mux);

        let front_end = Rc::new(GuiFrontEnd {
            connection,
            switching_workspaces: RefCell::new(false),
            spawned_mux_window: Rc::new(RefCell::new(HashMap::new())),
            known_windows: RefCell::new(BTreeMap::new()),
            osc22_cursor_shapes: RefCell::new(Osc22PerPaneCursorMap::new()),
            client_id: client_id.clone(),
            config_subscription: RefCell::new(None),
            workspace_reconcile_gate: Rc::new(Cell::new(WorkspaceReconcileGate::default())),
            workspace_reconcile_waiters: Rc::new(
                RefCell::new(WorkspaceReconcileWaiters::default()),
            ),
            workspace_reconcile_pass: Rc::new(RefCell::new(None)),
            osc52_dispatch_identity: Arc::new(()),
            layout_lifecycle: RefCell::new(
                crate::window_state_persist::startup_layout_state()
                    .ok()
                    .map(|startup| LayoutLifecycle {
                        startup,
                        owned: HashMap::new(),
                        restored: Default::default(),
                    }),
            ),
            applying_layout: Cell::new(false),
        });
        frankenterm_client::domain::install_layout_ready_observer(layout_attachment_ready)?;

        let prompt_frontend = Arc::downgrade(&front_end.osc52_dispatch_identity);
        let prompt_mux = Arc::downgrade(&mux);
        mux.set_osc52_prompt_handler(Arc::new(move |target, request| {
            if prompt_frontend.upgrade().is_none() || !request.is_pending() || !target.is_current()
            {
                return Err(wezterm_term::Osc52PromptError::Unavailable.into());
            }
            let reservation = match try_reserve_main_thread(
                MainThreadServiceClass::Input,
                FRONTEND_MAIN_THREAD_ESTIMATED_BYTES.saturating_add(request.decoded_bytes()),
            ) {
                MainThreadReservationOutcome::Reserved(reservation) => reservation,
                _ => return Err(wezterm_term::Osc52PromptError::Overloaded.into()),
            };
            let expected_frontend = prompt_frontend.clone();
            let mux_owner = prompt_mux.clone();
            reservation
                .spawn(async move {
                    let Some(frontend) = try_front_end() else {
                        request.cancel();
                        return;
                    };
                    if !std::sync::Weak::ptr_eq(
                        &expected_frontend,
                        &Arc::downgrade(&frontend.osc52_dispatch_identity),
                    ) || !target.is_current()
                        || !request.is_pending()
                    {
                        request.cancel();
                        return;
                    }
                    let Some(gui) = frontend.gui_window_for_mux_window(target.window_id()) else {
                        log::warn!(
                            "OSC 52 consent unavailable request={}: no originating GUI window",
                            request.id()
                        );
                        request.cancel();
                        return;
                    };
                    let exact_window = gui.window.clone();
                    gui.window
                        .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                            term_window.show_osc52_prompt(
                                target,
                                request,
                                mux_owner,
                                expected_frontend,
                                exact_window,
                            );
                        })));
                })
                .detach();
            Ok(())
        }));

        mux.subscribe(move |n| {
            if matches!(
                &n,
                MuxNotification::WindowOrderChanged { .. }
                    | MuxNotification::WindowTopologyChanged(_)
                    | MuxNotification::WindowCreated(_)
                    | MuxNotification::TabAddedToWindow { .. }
            ) && !frankenterm_client::domain::remote_layout_application_in_progress()
                && try_front_end().is_some_and(|frontend| !frontend.applying_layout.get())
            {
                schedule_layout_reconcile(false);
            }
            match n {
                MuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                } => {
                    if let Some(mux) = Mux::try_get() {
                        let active = mux.active_workspace();
                        if active == old_workspace || active == new_workspace {
                            if let Some(switcher) = WorkspaceSwitcher::new(&new_workspace) {
                                schedule_frontend_main_thread(
                                    MainThreadServiceClass::Topology,
                                    FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
                                    "workspace rename reconciliation",
                                    move || async move {
                                        drop(switcher);
                                    },
                                );
                            }
                        }
                    }
                }
                MuxNotification::WindowWorkspaceChanged { .. }
                | MuxNotification::ActiveWorkspaceChanged(_)
                | MuxNotification::WindowCreated(_)
                | MuxNotification::WindowRemoved(_)
                | MuxNotification::TabAddedToWindow { .. } => {
                    schedule_frontend_main_thread(
                        MainThreadServiceClass::Topology,
                        FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
                        "workspace state reconciliation",
                        move || async move {
                            if let Some(fe) = crate::frontend::try_front_end()
                                && !fe.is_switching_workspace()
                            {
                                fe.reconcile_workspace();
                            }
                        },
                    );
                }
                MuxNotification::WindowTopologyChanged(change)
                    if topology_needs_workspace_reconcile(&change) =>
                {
                    schedule_frontend_main_thread(
                        MainThreadServiceClass::Topology,
                        FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
                        "window topology reconciliation",
                        move || async move {
                            if let Some(fe) = crate::frontend::try_front_end()
                                && !fe.is_switching_workspace()
                            {
                                fe.reconcile_workspace();
                            }
                        },
                    );
                }
                MuxNotification::PaneFocused(pane_id) => {
                    schedule_frontend_main_thread(
                        MainThreadServiceClass::Input,
                        FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
                        "focused pane reconciliation",
                        move || async move {
                            if let Some(mux) = Mux::try_get()
                                && let Err(err) = mux.focus_pane_and_containing_tab(pane_id)
                            {
                                log::error!("error reconciling PaneFocused notification: {err:#}");
                            }
                            if let Some(fe) = crate::frontend::try_front_end() {
                                fe.apply_osc22_cursor_shape_for_pane(pane_id);
                            }
                        },
                    );
                }
                MuxNotification::PaneRemoved(pane_id) => {
                    if let Some(fe) = crate::frontend::try_front_end() {
                        fe.osc22_cursor_shapes
                            .borrow_mut()
                            .forget(pane_id as u64);
                    }
                }
                MuxNotification::TabTitleChanged { .. } => {}
                MuxNotification::WindowTitleChanged { .. } => {}
                MuxNotification::TabResized(_) => {}
                MuxNotification::WindowInvalidated(_)
                | MuxNotification::WindowTopologyChanged(_)
                | MuxNotification::WindowOrderChanged { .. } => {}
                MuxNotification::PaneOutput(_) => {}
                MuxNotification::SynchronizedOutput { .. } => {}
                MuxNotification::PaneAdded(_) => {}
                MuxNotification::FloatingPaneSpawnCommitted(_) => {
                    // Exact focus and tab ownership were already committed by
                    // the mux transaction. Never queue numeric-id focus
                    // reconciliation here: it could override a newer user tab
                    // switch or target a same-id successor.
                }
                MuxNotification::Alert {
                    pane_id,
                    alert:
                        Alert::ToastNotification {
                            title,
                            body,
                            focus,
                        },
                } => {
                    let Some(mux) = Mux::try_get() else {
                        return true;
                    };

                    if let Some((_domain, window_id, tab_id)) = mux.resolve_pane_id(pane_id) {
                        let config = config::configuration();

                        if let Some((_fdomain, f_window, f_tab, f_pane)) =
                            mux.resolve_focused_pane(&client_id)
                        {
                            let show = match config.notification_handling {
                                NotificationHandling::NeverShow => false,
                                NotificationHandling::AlwaysShow => true,
                                NotificationHandling::SuppressFromFocusedPane => f_pane != pane_id,
                                NotificationHandling::SuppressFromFocusedTab => f_tab != tab_id,
                                NotificationHandling::SuppressFromFocusedWindow => {
                                    f_window != window_id
                                }
                            };

                            if show {
                                let message = if title.is_none() { "" } else { &body };
                                let title = title.as_ref().unwrap_or(&body);
                                if let Some(action) = terminal_toast_action(focus, pane_id) {
                                    persistent_toast_notification_with_action(
                                        title, message, action,
                                    );
                                } else {
                                    persistent_toast_notification(title, message);
                                }
                            }
                        }
                    }
                }
                MuxNotification::Alert {
                    pane_id: _,
                    alert: Alert::Bell | Alert::Progress(_),
                } => {
                    // Handled via TermWindowNotif; NOP it here.
                }
                MuxNotification::Alert {
                    pane_id: _,
                    alert:
                        Alert::OutputSinceFocusLost
                        | Alert::PaletteChanged
                        | Alert::CurrentWorkingDirectoryChanged
                        | Alert::WindowTitleChanged(_)
                        | Alert::TabTitleChanged(_)
                        | Alert::IconTitleChanged(_)
                        | Alert::ImageAltText { .. }
                        | Alert::SetUserVar { .. }
                        // ft-fy4ty: SetProfileRequested is dispatched
                        // to a confirmation prompt at this layer; the
                        // GUI continuation bead (ft-tzusd) wires the
                        // actual modal. Until then we accept the
                        // alert silently to preserve the safer
                        // default.
                        | Alert::SetProfileRequested { .. }
                } => {}
                MuxNotification::Alert {
                    pane_id,
                    alert: Alert::MouseShapeRequested { shape },
                } => {
                    if let Some(fe) = crate::frontend::try_front_end() {
                        fe.record_osc22_cursor_shape(pane_id, &shape);
                    }
                }
                MuxNotification::Empty => {
                    if config::configuration().quit_when_all_windows_are_closed {
                        let mux_owner = mux_owner.clone();
                        schedule_frontend_main_thread(
                            MainThreadServiceClass::Topology,
                            FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
                            "empty mux termination",
                            move || async move {
                                let Some(notifying_mux) = mux_owner.upgrade() else {
                                    return;
                                };
                                let is_current_owner = Mux::try_get()
                                    .is_some_and(|current| Arc::ptr_eq(&current, &notifying_mux));
                                if is_current_owner
                                    && notifying_mux.is_empty()
                                    && mux::activity::Activity::count_for_mux(&notifying_mux) == 0
                                {
                                    log::trace!("Exact mux is still empty; terminate gui");
                                    if let Some(conn) = Connection::get() {
                                        conn.terminate_message_loop();
                                    }
                                }
                            },
                        );
                    }
                }
                MuxNotification::SaveToDownloads { name, data } => {
                    if !config::configuration().allow_download_protocols {
                        log::error!(
                            "Ignoring download request for {:?}, \
                                 as allow_download_protocols=false",
                            name
                        );
                    } else if let Err(err) = crate::download::save_to_downloads(name, &*data) {
                        log::error!("save_to_downloads: {:#}", err);
                    }
                }
                MuxNotification::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                } => {
                    let estimated_bytes = FRONTEND_MAIN_THREAD_ESTIMATED_BYTES.saturating_add(
                        clipboard.as_ref().map_or(0, String::len),
                    );
                    schedule_frontend_main_thread(
                        MainThreadServiceClass::Input,
                        estimated_bytes,
                        "clipboard assignment",
                        move || async move {
                            log::trace!(
                                "enqueue clipboard in pane {} selection={:?} bytes={}",
                                pane_id,
                                selection,
                                clipboard.as_ref().map_or(0, String::len)
                            );
                            let Some(fe) = crate::frontend::try_front_end() else {
                                return;
                            };
                            if let Some(window) = fe.known_windows.borrow().keys().next() {
                                window.set_clipboard(
                                    match selection {
                                        ClipboardSelection::Clipboard => Clipboard::Clipboard,
                                        ClipboardSelection::PrimarySelection => {
                                            Clipboard::PrimarySelection
                                        }
                                    },
                                    clipboard.unwrap_or_else(String::new),
                                );
                            } else {
                                log::error!("Cannot assign clipboard as there are no windows");
                            };
                        },
                    );
                }
            }
            true
        })
        .context("allocate GUI frontend mux subscription")?;
        // Re-evaluate the config so that folks that are using
        // `wezterm.gui.get_appearance()` can have that take effect
        // before any windows are created
        config::reload();

        // And build the initial menu bar — synchronously, BEFORE we hand
        // control to AppKit's run loop. Deferring this through the bounded scheduler
        // queues it for the next main-thread tick, but the implicit AE-open
        // event AppKit dispatches at launch arrives first; AppKit then walks
        // [NSApp mainMenu] in -[NSApplication _hasOpenMenuItem] / _doOpenUntitled
        // and segfaults because the menu hasn't been built yet.
        crate::commands::CommandDef::recreate_menubar(&config::configuration());

        Ok(front_end)
    }

    fn app_event_handler(event: ApplicationEvent) {
        log::trace!("Got app event {event:?}");
        match event {
            ApplicationEvent::OpenCommandScript(file_name) => {
                let quoted_file_name = match shlex::try_quote(&file_name) {
                    Ok(name) => name.into_owned(),
                    Err(_) => {
                        log::error!(
                            "OpenCommandScript: {file_name} has embedded NUL bytes and
                             cannot be launched via the shell"
                        );
                        return;
                    }
                };
                schedule_frontend_main_thread(
                    MainThreadServiceClass::Topology,
                    FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
                    "open command script",
                    move || async move {
                        use config::keyassignment::SpawnTabDomain;
                        use wezterm_term::TerminalSize;

                        // We send the script to execute to the shell on stdin, rather than ask the
                        // shell to execute it directly, so that we start the shell and read in the
                        // user's rc files before running the script.  Without this, wezterm on macOS
                        // is launched with a default and very anemic path, and that is frustrating for
                        // users.

                        let Some(mux) = Mux::try_get() else {
                            log::error!("OpenCommandScript: mux singleton is not available");
                            return;
                        };
                        let window_id = None;
                        let pane_id = None;
                        let cmd = None;
                        let cwd = None;
                        let workspace = mux.active_workspace();

                        match mux
                            .spawn_tab_or_window(
                                window_id,
                                SpawnTabDomain::DomainName("local".to_string()),
                                cmd,
                                cwd,
                                TerminalSize::default(),
                                pane_id,
                                workspace,
                                None, // optional position
                                mux.active_identity(),
                            )
                            .await
                        {
                            Ok((_tab, pane, _window_id)) => {
                                log::trace!("Spawned {file_name} as pane_id {}", pane.pane_id());
                                let mut writer = pane.writer();
                                write!(writer, "{quoted_file_name} ; exit\n").ok();
                            }
                            Err(err) => {
                                log::error!("Failed to spawn {file_name}: {err:#?}");
                            }
                        };
                    },
                );
            }
            ApplicationEvent::PerformKeyAssignment(action) => {
                // We should only get here when there are no windows open
                // and the user picks an action from the menubar.
                // This is not currently possible, but could be in the
                // future.

                fn spawn_command(spawn: &SpawnCommand, spawn_where: SpawnWhere) {
                    let config = config::configuration();
                    let dpi = config.dpi.unwrap_or_else(::window::default_dpi);
                    let size =
                        config.initial_size(dpi as u32, crate::cell_pixel_dims(&config, dpi).ok());
                    let term_config = Arc::new(config::TermConfig::with_config(config));

                    crate::spawn::spawn_command_impl(spawn, spawn_where, size, None, term_config)
                }

                match action {
                    KeyAssignment::QuitApplication => {
                        // If we get here, there are no windows that could have received
                        // the QuitApplication command, therefore it must be ok to quit
                        // immediately
                        if let Some(conn) = Connection::get() {
                            conn.terminate_message_loop();
                        }
                    }
                    KeyAssignment::SpawnWindow => {
                        spawn_command(&SpawnCommand::default(), SpawnWhere::NewWindow);
                    }
                    KeyAssignment::SpawnTab(spawn_where) => {
                        spawn_command(
                            &SpawnCommand {
                                domain: spawn_where,
                                ..Default::default()
                            },
                            SpawnWhere::NewWindow,
                        );
                    }
                    KeyAssignment::SpawnCommandInNewTab(spawn) => {
                        spawn_command(&spawn, SpawnWhere::NewTab);
                    }
                    KeyAssignment::SpawnCommandInNewWindow(spawn) => {
                        spawn_command(&spawn, SpawnWhere::NewWindow);
                    }
                    _ => {
                        log::warn!("unhandled perform: {action:?}");
                    }
                }
            }
        }
    }

    pub fn run_forever(&self) -> anyhow::Result<()> {
        self.connection
            .run_message_loop()
            .context("running message loop")
    }

    pub fn gui_windows(&self) -> Vec<GuiWin> {
        let windows = self.known_windows.borrow();
        let mut windows: Vec<GuiWin> = windows
            .iter()
            .map(|(window, &mux_window_id)| GuiWin {
                mux_window_id,
                window: window.clone(),
            })
            .collect();
        windows.sort_by(|a, b| a.window.cmp(&b.window));
        windows
    }

    pub fn reconcile_workspace(&self) -> Future<()> {
        let mut waiters = self.workspace_reconcile_waiters.borrow_mut();
        if waiters.waiter_count() >= MAX_RECONCILE_WAITERS {
            return Future::err(Error::msg(format!(
                "workspace reconciliation waiter count would exceed {MAX_RECONCILE_WAITERS}"
            )));
        }

        let mut promise = Promise::new();
        let future = promise.get_future().unwrap();

        let mut gate = self.workspace_reconcile_gate.get();
        let start_pass = gate.request_pass();
        self.workspace_reconcile_gate.set(gate);
        waiters.push(start_pass, promise);
        drop(waiters);
        if start_pass {
            self.run_workspace_reconcile_pass();
        }
        future
    }

    fn run_workspace_reconcile_pass(&self) {
        let pass = PendingWorkspaceReconcile::new(
            &self.workspace_reconcile_pass,
            &self.workspace_reconcile_gate,
            &self.workspace_reconcile_waiters,
        );
        let Some(mux) = Mux::try_get() else {
            log::warn!("cannot reconcile workspace: mux singleton is not available");
            self.finish_workspace_reconcile_pass(pass, Some("mux is unavailable"));
            return;
        };
        let workspace = mux.active_workspace_for_client(&self.client_id);

        if mux.is_workspace_empty(&workspace) {
            // We don't want to silently kill off things that might
            // be running in other workspaces, so let's pick one
            // and activate it
            if self.is_switching_workspace() {
                self.finish_workspace_reconcile_pass(pass, None);
                return;
            }
            for workspace in mux.iter_workspaces() {
                if !mux.is_workspace_empty(&workspace) {
                    mux.set_active_workspace_for_client(&self.client_id, &workspace);
                    log::debug!("using {} instead, as it is not empty", workspace);
                    break;
                }
            }
        }

        let workspace = mux.active_workspace_for_client(&self.client_id);
        log::debug!("workspace is {}, fixup windows", workspace);

        // Freeze one restore value for this entire reconciliation cohort before
        // notifying, repurposing, or showing any GUI window. Window callbacks
        // can enqueue newer state, but they must not make a single startup
        // cohort observe a mixture of launch-time and callback-time values.
        let saved_window_state =
            crate::window_state_persist::load_startup_for_workspace(&workspace);
        let mut mux_windows = mux.iter_windows_in_workspace(&workspace);
        // WindowCreated can precede the first tab's asynchronous spawn. Do not
        // create or repurpose a view using invented default geometry; the tab
        // attachment notification will request another reconciliation pass.
        // Keep existing views while their tabs are being changed.
        mux_windows.retain(|&id| {
            self.has_mux_window(id)
                || (crate::termwindow::initial_native_window_size(&mux, id).is_some()
                    && mux
                        .window_order_snapshot(id)
                        .ok()
                        .flatten()
                        .is_some_and(|order| {
                            order.ordered_tabs().iter().all(|tab| {
                                tab.iter_panes().iter().all(|pane| {
                                    mux.get_domain(pane.pane.domain_id()).is_some_and(|domain| {
                                        domain
                                            .downcast_ref::<ClientDomain>()
                                            .is_none_or(|client| !client.layout_restore_pending())
                                    })
                                })
                            })
                        }))
        });

        // First, repurpose existing windows.
        // Note that both iter_windows_in_workspace and self.known_windows have a
        // deterministic iteration order, so switching back and forth should result
        // in a consistent mux <-> gui window mapping.
        let known_windows = std::mem::take(&mut *self.known_windows.borrow_mut());
        let mut windows = BTreeMap::new();
        let mut unused = BTreeMap::new();

        for (window, window_id) in known_windows.into_iter() {
            if let Some(idx) = mux_windows.iter().position(|&id| id == window_id) {
                // it already points to the desired mux window
                windows.insert(window, window_id);
                mux_windows.remove(idx);
            } else {
                unused.insert(window, window_id);
            }
        }

        let mut mux_windows = mux_windows.into_iter();

        for (window, old_id) in unused.into_iter() {
            if let Some(mux_window_id) = mux_windows.next() {
                window.notify(TermWindowNotif::SwitchToMuxWindow(mux_window_id));
                windows.insert(window, mux_window_id);
            } else {
                // We have more windows than are in the new workspace;
                // we no longer need this one!
                window.close();
                self.spawned_mux_window.borrow_mut().remove(&old_id);
            }
        }

        log::trace!("reconcile: windows -> {:?}", windows);
        *self.known_windows.borrow_mut() = windows;

        // then spawn any new windows that are needed
        let reservation = match try_reserve_main_thread(
            MainThreadServiceClass::Topology,
            FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
        ) {
            MainThreadReservationOutcome::Reserved(reservation) => reservation,
            rejected => {
                self.finish_workspace_reconcile_pass(
                    pass,
                    Some("workspace reconciliation scheduling was rejected"),
                );
                log::error!(
                    "GUI main-thread scheduler rejected workspace reconciliation suffix; released the exact pass gate for retry: {rejected:?}"
                );
                return;
            }
        };
        reservation
            .spawn_local(async move {
                let mut failure = None;
                while let Some(mux_window_id) = mux_windows.next() {
                    let Some(fe) = try_front_end() else {
                        return;
                    };
                    if !Rc::ptr_eq(&fe.workspace_reconcile_pass, &pass.current) {
                        return;
                    }
                    if fe.has_mux_window(mux_window_id) {
                        continue;
                    }
                    if crate::termwindow::initial_native_window_size(&mux, mux_window_id).is_none()
                    {
                        continue;
                    }
                    let Some(_pending_creation) =
                        PendingWindowCreation::try_begin(&fe.spawned_mux_window, mux_window_id)
                    else {
                        continue;
                    };
                    log::trace!("Creating TermWindow for mux_window_id={}", mux_window_id);
                    if let Err(err) =
                        TermWindow::new_window(mux_window_id, workspace.clone(), saved_window_state)
                            .await
                    {
                        failure = Some("native window creation failed");
                        // Native allocation/render initialization can fail while
                        // the mux window still owns live sessions. A failed view
                        // must not close those sessions; release the creation
                        // marker so a later reconciliation can retry the view.
                        metrics::counter!("gui.window_creation_failed.total").increment(1);
                        log::error!(
                            "Failed to create native window for mux window {mux_window_id}; \
                             preserving its sessions for retry: {err:#}"
                        );
                    }
                }
                if let Some(fe) = try_front_end() {
                    fe.finish_workspace_reconcile_pass(pass, failure);
                }
            })
            .detach();
    }

    fn finish_workspace_reconcile_pass(
        &self,
        pass: PendingWorkspaceReconcile,
        failure: Option<&'static str>,
    ) {
        if !Rc::ptr_eq(&self.workspace_reconcile_pass, &pass.current) {
            return;
        }
        if pass.finish(failure) == Some(true) {
            self.run_workspace_reconcile_pass();
        }
    }

    fn has_mux_window(&self, mux_window_id: MuxWindowId) -> bool {
        for &mux_id in self.known_windows.borrow().values() {
            if mux_id == mux_window_id {
                return true;
            }
        }
        false
    }

    pub fn switch_workspace(&self, workspace: &str) {
        if let Some(mux) = Mux::try_get() {
            mux.set_active_workspace_for_client(&self.client_id, workspace);
        } else {
            log::warn!("cannot switch workspace to {workspace}: mux singleton is not available");
        }
        *self.switching_workspaces.borrow_mut() = false;
        self.reconcile_workspace();
    }

    pub fn record_known_window(&self, window: Window, mux_window_id: MuxWindowId) {
        self.known_windows
            .borrow_mut()
            .insert(window, mux_window_id);
        if !self.is_switching_workspace() {
            self.reconcile_workspace();
        }
    }

    pub fn forget_known_window(&self, window: &Window) {
        self.known_windows.borrow_mut().remove(window);
        if !self.is_switching_workspace() {
            self.reconcile_workspace();
        }
    }

    pub fn is_switching_workspace(&self) -> bool {
        *self.switching_workspaces.borrow()
    }

    #[allow(dead_code)]
    pub fn gui_window_for_mux_window(&self, mux_window_id: MuxWindowId) -> Option<GuiWin> {
        let windows = self.known_windows.borrow();
        for (window, v) in windows.iter() {
            if *v == mux_window_id {
                return Some(GuiWin {
                    mux_window_id,
                    window: window.clone(),
                });
            }
        }
        None
    }

    fn record_osc22_cursor_shape(&self, pane_id: mux::pane::PaneId, shape: &str) {
        let Some(slug) = cursor_shape_slug_from_osc22_request(shape) else {
            log::debug!("ignoring unsupported OSC 22 cursor shape request: {shape:?}");
            return;
        };
        let prior = self
            .osc22_cursor_shapes
            .borrow_mut()
            .set(pane_id as u64, slug);
        self.apply_osc22_cursor_shape_for_pane(pane_id);
        if prior != Some(slug) {
            persistent_toast_notification(
                "Cursor shape changed",
                osc22_accessibility_announcement(slug).as_str(),
            );
        }
        log::debug!(
            "OSC 22 cursor shape for pane {pane_id} is now {}",
            slug.slug()
        );
    }

    fn apply_osc22_cursor_shape_for_pane(&self, pane_id: mux::pane::PaneId) {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        let Some((_domain, window_id, _tab_id)) = mux.resolve_pane_id(pane_id) else {
            return;
        };
        let shape = self.osc22_cursor_shapes.borrow().get(pane_id as u64);
        if let Some(gui_window) = self.gui_window_for_mux_window(window_id) {
            gui_window
                .window
                .set_cursor(Some(mouse_cursor_for_osc22_shape(shape)));
        }
    }
}

pub(crate) fn osc52_frontend_is_current(expected: &std::sync::Weak<()>) -> bool {
    try_front_end().is_some_and(|frontend| {
        std::sync::Weak::ptr_eq(expected, &Arc::downgrade(&frontend.osc52_dispatch_identity))
    })
}

#[must_use]
fn cursor_shape_slug_from_osc22_request(shape: &str) -> Option<CursorShapeSlug> {
    let normalized = shape.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    match normalized.as_str() {
        "" | "auto" | "default" | "arrow" => Some(CursorShapeSlug::Default),
        "block" | "block_blinking" | "blinking_block" => Some(CursorShapeSlug::BlockBlinking),
        "block_steady" | "steady_block" => Some(CursorShapeSlug::BlockSteady),
        "underline" | "underline_blinking" | "blinking_underline" => {
            Some(CursorShapeSlug::UnderlineBlinking)
        }
        "underline_steady" | "steady_underline" => Some(CursorShapeSlug::UnderlineSteady),
        "bar" | "beam" | "ibeam" | "text" | "bar_blinking" | "blinking_bar" => {
            Some(CursorShapeSlug::BarBlinking)
        }
        "bar_steady" | "steady_bar" => Some(CursorShapeSlug::BarSteady),
        _ => None,
    }
}

#[must_use]
fn mouse_cursor_for_osc22_shape(shape: CursorShapeSlug) -> MouseCursor {
    match shape {
        CursorShapeSlug::Default
        | CursorShapeSlug::BlockBlinking
        | CursorShapeSlug::BlockSteady => MouseCursor::Arrow,
        CursorShapeSlug::UnderlineBlinking
        | CursorShapeSlug::UnderlineSteady
        | CursorShapeSlug::BarBlinking
        | CursorShapeSlug::BarSteady => MouseCursor::Text,
    }
}

#[must_use]
fn osc22_accessibility_announcement(shape: CursorShapeSlug) -> String {
    format!("Cursor shape {}", shape.slug().replace('_', " "))
}

fn terminal_toast_action(
    focus: bool,
    pane_id: mux::pane::PaneId,
) -> Option<ToastNotificationAction> {
    focus.then(|| {
        ToastNotificationAction::new("Focus", move || {
            schedule_frontend_main_thread(
                MainThreadServiceClass::Interactive,
                FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
                "terminal toast focus",
                move || async move {
                    focus_terminal_toast_source(pane_id);
                },
            );
        })
    })
}

fn focus_terminal_toast_source(pane_id: mux::pane::PaneId) {
    let Some(mux) = Mux::try_get() else {
        log::warn!("cannot focus toast source pane {pane_id}: mux singleton is not available");
        return;
    };
    let Some((_domain, window_id, _tab_id)) = mux.resolve_pane_id(pane_id) else {
        log::warn!("cannot focus toast source pane {pane_id}: pane is no longer in the mux");
        return;
    };

    if let Err(err) = mux.focus_pane_and_containing_tab(pane_id) {
        log::error!("cannot focus toast source pane {pane_id}: {err:#}");
        return;
    }

    let Some(front_end) = crate::frontend::try_front_end() else {
        log::warn!("cannot raise toast source window for pane {pane_id}: frontend is unavailable");
        return;
    };
    let Some(gui_window) = front_end.gui_window_for_mux_window(window_id) else {
        log::warn!("cannot raise toast source window for pane {pane_id}: GUI window not found");
        return;
    };

    gui_window.window.focus();
    front_end.apply_osc22_cursor_shape_for_pane(pane_id);
}

#[cfg(test)]
mod tests {
    use super::{
        CursorShapeSlug, MouseCursor, cursor_shape_slug_from_osc22_request,
        mouse_cursor_for_osc22_shape, osc22_accessibility_announcement, terminal_toast_action,
    };

    #[cfg(unix)]
    #[test]
    fn layout_input_target_rejects_a_pane_moved_to_another_window() {
        use mux::pane::Pane;
        use std::sync::Arc;

        let owner = Arc::new(mux::Mux::new(None));
        let _activity = mux::activity::Activity::new_for_mux(&owner);
        let left = owner.new_empty_window(None, None);
        let right = owner.new_empty_window(None, None);
        let size = wezterm_term::TerminalSize::default();
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let writer = pair.master.take_writer().unwrap();
        let terminal = wezterm_term::Terminal::new(
            size,
            Arc::new(config::TermConfig::new_for_pane(
                998_301,
                998_301,
                [0x31; 16],
                "layout input test".to_owned(),
            )),
            "FrankenTerm",
            "layout-input-test",
            Box::new(Vec::<u8>::new()),
        );
        let child = pair
            .slave
            .spawn_command(portable_pty::CommandBuilder::new("/bin/cat"))
            .unwrap();
        let pane: Arc<dyn Pane> = Arc::new(mux::localpane::LocalPane::new(
            998_301,
            terminal,
            child,
            pair.master,
            writer,
            998_301,
            [0x31; 16],
            "layout input test".to_owned(),
        ));
        struct RetireChild(Arc<dyn Pane>);
        impl Drop for RetireChild {
            fn drop(&mut self) {
                self.0.kill();
            }
        }
        let _child = RetireChild(Arc::clone(&pane));
        let tab = Arc::new(mux::tab::Tab::new(&size));
        tab.assign_pane(&pane);
        owner.add_tab_and_active_pane(&tab).unwrap();
        owner.add_tab_to_window(&tab, *left).unwrap();
        assert!(super::layout_pane_belongs_to_window(&owner, *left, &pane));
        assert!(!super::layout_pane_belongs_to_window(&owner, *right, &pane));

        let left_order = owner.window_order_snapshot(*left).unwrap().unwrap();
        let right_order = owner.window_order_snapshot(*right).unwrap().unwrap();
        owner
            .apply_window_order_mirrors(vec![
                mux::WindowOrderMirror {
                    expected: left_order,
                    ordered_tabs: Vec::new(),
                    active_tab: None,
                },
                mux::WindowOrderMirror {
                    expected: right_order,
                    ordered_tabs: vec![Arc::clone(&tab)],
                    active_tab: Some(Arc::clone(&tab)),
                },
            ])
            .unwrap();
        assert!(!super::layout_pane_belongs_to_window(&owner, *left, &pane));
        assert!(super::layout_pane_belongs_to_window(&owner, *right, &pane));
        left.cancel();
        right.cancel();
    }

    #[test]
    fn mixed_layout_restore_uses_one_atomic_mux_transaction_and_rejects_old_session_slots() {
        use super::{LayoutLifecycle, LiveLayout, MixedDomainLayoutOverlay};
        use mux::activity::Activity;
        use mux::tab::Tab;
        use mux::{Mux, MuxNotification};
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        let mux = Arc::new(Mux::new(None));
        let _activity = Activity::new_for_mux(&mux);
        let left = mux.new_empty_window(None, None);
        let right = mux.new_empty_window(None, None);
        let ids = [*left, *right];
        let tabs: Vec<_> = (0..4)
            .map(|_| Arc::new(Tab::new(&Default::default())))
            .collect();
        for (index, tab) in tabs.iter().enumerate() {
            mux.add_tab_no_panes(tab).unwrap();
            mux.add_tab_to_window(tab, ids[index / 2]).unwrap();
        }
        let temp = tempfile::tempdir().unwrap();
        let mut startup =
            crate::window_state_persist::load_snapshot_at(&temp.path().join("layout.json"))
                .unwrap();
        let live = LiveLayout::capture(&mux, &startup).unwrap();
        let slots: Vec<_> = tabs.iter().map(|tab| live.slots[&tab.tab_id()]).collect();
        let first_id = startup.new_layout_window_id().unwrap();
        let second_id = startup.new_layout_window_id().unwrap();
        startup.overlays = vec![
            MixedDomainLayoutOverlay::new(
                first_id,
                "default",
                1,
                vec![slots[3], slots[0]],
                Some(slots[0]),
            )
            .unwrap(),
            MixedDomainLayoutOverlay::new(
                second_id,
                "default",
                1,
                vec![slots[1], slots[2]],
                Some(slots[2]),
            )
            .unwrap(),
        ];
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_callback = Arc::clone(&observed);
        let owner = Arc::downgrade(&mux);
        mux.subscribe(move |event| {
            if let MuxNotification::WindowTopologyChanged(change) = event {
                if change.affects_window(ids[0]) && change.affects_window(ids[1]) {
                    let mux = owner.upgrade().unwrap();
                    observed_callback.lock().unwrap().push(ids.map(|id| {
                        mux.window_order_snapshot(id)
                            .unwrap()
                            .unwrap()
                            .ordered_tab_ids()
                            .collect::<Vec<_>>()
                    }));
                }
            }
            true
        })
        .unwrap();
        let mut lifecycle = LayoutLifecycle {
            startup,
            owned: HashMap::new(),
            restored: Default::default(),
        };
        lifecycle.restore(&mux, &live).unwrap();
        let expected = [
            vec![tabs[1].tab_id(), tabs[2].tab_id()],
            vec![tabs[3].tab_id(), tabs[0].tab_id()],
        ];
        assert_eq!(*observed.lock().unwrap(), vec![expected.clone()]);
        assert_eq!(
            mux.window_order_snapshot(ids[0])
                .unwrap()
                .unwrap()
                .active_tab_id(),
            Some(tabs[2].tab_id())
        );
        assert_eq!(
            mux.window_order_snapshot(ids[1])
                .unwrap()
                .unwrap()
                .active_tab_id(),
            Some(tabs[0].tab_id())
        );
        assert_eq!(lifecycle.owned[&ids[1]].overlay.window_id(), first_id);
        assert_eq!(lifecycle.owned[&ids[0]].overlay.window_id(), second_id);
        lifecycle.restored.clear();
        let old_session = super::StableTabSlot::local(
            super::StableLocalSessionId::from_bytes(*uuid::Uuid::new_v4().as_bytes()),
            super::StableLocalTabId::from_bytes(*tabs[0].durable_id().as_bytes()),
        );
        lifecycle.startup.overlays = vec![
            MixedDomainLayoutOverlay::new(
                first_id,
                "default",
                2,
                vec![old_session],
                Some(old_session),
            )
            .unwrap(),
        ];
        observed.lock().unwrap().clear();
        lifecycle.restore(&mux, &live).unwrap();
        assert!(observed.lock().unwrap().is_empty());
        for (id, expected) in ids.into_iter().zip(expected) {
            assert_eq!(
                mux.window_order_snapshot(id)
                    .unwrap()
                    .unwrap()
                    .ordered_tab_ids()
                    .collect::<Vec<_>>(),
                expected
            );
        }
        left.cancel();
        right.cancel();
    }

    #[test]
    fn native_window_startup_waits_for_attachment_to_an_already_published_window() {
        use mux::activity::Activity;
        use mux::tab::Tab;
        use mux::{Mux, MuxNotification};
        use std::sync::{Arc, Mutex};
        use wezterm_term::TerminalSize;

        let mux = Arc::new(Mux::new(None));
        let activity = Activity::new_for_mux(&mux);
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&notifications);
        mux.subscribe(move |notification| {
            observed.lock().unwrap().push(notification);
            true
        })
        .unwrap();
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        assert!(crate::termwindow::initial_native_window_size(&mux, window_id).is_none());
        drop(window);
        assert!(
            notifications.lock().unwrap().iter().any(
                |event| matches!(event, MuxNotification::WindowCreated(id) if *id == window_id)
            )
        );
        notifications.lock().unwrap().clear();

        let size = TerminalSize {
            cols: 60,
            rows: 20,
            pixel_width: 960,
            pixel_height: 720,
            dpi: 144,
        };
        let tab = Arc::new(Tab::new(&size));
        mux.add_tab_no_panes(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        assert_eq!(
            crate::termwindow::initial_native_window_size(&mux, window_id),
            Some(size)
        );
        let events = notifications.lock().unwrap();
        let change = events
            .iter()
            .find_map(|event| match event {
                MuxNotification::WindowTopologyChanged(change) => Some(change),
                _ => None,
            })
            .expect("attachment must publish a topology transaction");
        assert!(change.created_windows().is_empty());
        assert!(change.removed_windows().is_empty());
        assert_eq!(change.attached_tabs(), &[(tab.tab_id(), window_id)]);
        assert!(
            super::topology_needs_workspace_reconcile(change),
            "a tab arriving after WindowCreated must wake deferred native creation"
        );
        drop(events);
        // No global mux, frontend, native window or event loop is initialized.
        drop(mux);
        drop(activity);
    }

    #[test]
    fn native_window_startup_has_no_default_geometry_after_tab_removal() {
        use mux::Mux;
        use mux::activity::Activity;
        use mux::tab::Tab;
        use std::sync::Arc;
        use wezterm_term::TerminalSize;

        let mux = Arc::new(Mux::new(None));
        let activity = Activity::new_for_mux(&mux);
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        let size = TerminalSize {
            cols: 60,
            rows: 20,
            ..TerminalSize::default()
        };
        let tab = Arc::new(Tab::new(&size));
        mux.add_tab_no_panes(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        assert_eq!(
            crate::termwindow::initial_native_window_size(&mux, window_id),
            Some(size)
        );
        assert!(mux.remove_tab(tab.tab_id()).is_some());
        assert!(crate::termwindow::initial_native_window_size(&mux, window_id).is_none());
        window.cancel();
        assert!(crate::termwindow::initial_native_window_size(&mux, window_id).is_none());
        drop(mux);
        drop(activity);
    }

    #[test]
    fn pending_native_window_closes_only_unpublished_views() {
        use super::PendingNativeWindow;
        use std::cell::Cell;
        use std::future::Future;
        use std::rc::Rc;
        use std::task::{Context, Poll};

        let closed = Rc::new(Cell::new(0));
        let count = Rc::clone(&closed);
        let failed = PendingNativeWindow::new(move || count.set(count.get() + 1));
        let mut failure = Box::pin(async move {
            let _view = failed;
            Err::<(), _>(anyhow::anyhow!("post-allocation renderer failure"))
        });
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            failure.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        drop(failure);
        assert_eq!(closed.get(), 1);

        let count = Rc::clone(&closed);
        let cancelled = PendingNativeWindow::new(move || count.set(count.get() + 1));
        let mut initialization = Box::pin(async move {
            let _view = cancelled;
            std::future::pending::<()>().await;
        });
        assert_eq!(initialization.as_mut().poll(&mut cx), Poll::Pending);
        drop(initialization);
        assert_eq!(closed.get(), 2);

        let count = Rc::clone(&closed);
        PendingNativeWindow::new(move || count.set(count.get() + 1)).publish();
        assert_eq!(closed.get(), 2, "published views must remain open");
    }

    #[test]
    fn pending_workspace_reconcile_cancellation_releases_gate_and_all_waiters() {
        use super::PendingWorkspaceReconcile;
        use frankenterm_gui::workspace_reconcile::{
            WorkspaceReconcileGate, WorkspaceReconcileWaiters,
        };
        use promise::Promise;
        use std::cell::{Cell, RefCell};
        use std::future::Future;
        use std::pin::Pin;
        use std::rc::Rc;
        use std::task::{Context, Poll};

        let current = Rc::new(RefCell::new(None));
        let gate = Rc::new(Cell::new(WorkspaceReconcileGate::default()));
        let waiters = Rc::new(RefCell::new(WorkspaceReconcileWaiters::default()));
        let mut first = Promise::new();
        let mut first_result = first.get_future().unwrap();
        let mut next = Promise::new();
        let mut next_result = next.get_future().unwrap();
        let mut requested = gate.get();
        assert!(requested.request_pass());
        waiters.borrow_mut().push(true, first);
        assert!(!requested.request_pass());
        waiters.borrow_mut().push(false, next);
        gate.set(requested);
        let pass = PendingWorkspaceReconcile::new(&current, &gate, &waiters);
        let mut task = Box::pin(async move {
            let _pass = pass;
            std::future::pending::<()>().await;
        });
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Pending);
        drop(task);
        for result in [&mut first_result, &mut next_result] {
            assert!(matches!(
                Pin::new(result).poll(&mut cx),
                Poll::Ready(Err(_))
            ));
        }
        assert_eq!(waiters.borrow().waiter_count(), 0);
        assert!(current.borrow().is_none());

        let mut requested = gate.get();
        assert!(
            requested.request_pass(),
            "cancelled pass must not wedge retry"
        );
        gate.set(requested);
        let mut retry = Promise::new();
        let mut retry_result = retry.get_future().unwrap();
        waiters.borrow_mut().push(true, retry);
        let retry_pass = PendingWorkspaceReconcile::new(&current, &gate, &waiters);
        assert_eq!(retry_pass.finish(None), Some(false));
        assert!(matches!(
            Pin::new(&mut retry_result).poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(gate.get(), WorkspaceReconcileGate::default());

        let mut requested = gate.get();
        assert!(requested.request_pass());
        gate.set(requested);
        let mut failure = Promise::new();
        let mut failure_result = failure.get_future().unwrap();
        waiters.borrow_mut().push(true, failure);
        let failed_pass = PendingWorkspaceReconcile::new(&current, &gate, &waiters);
        assert_eq!(
            failed_pass.finish(Some("native window creation failed")),
            Some(false)
        );
        assert!(matches!(
            Pin::new(&mut failure_result).poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        assert_eq!(gate.get(), WorkspaceReconcileGate::default());
    }

    #[test]
    fn pending_workspace_reconcile_stale_drop_cannot_cancel_the_successor() {
        use super::PendingWorkspaceReconcile;
        use frankenterm_gui::workspace_reconcile::{
            WorkspaceReconcileGate, WorkspaceReconcileWaiters,
        };
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;

        let current = Rc::new(RefCell::new(None));
        let gate = Rc::new(Cell::new(WorkspaceReconcileGate::default()));
        let waiters = Rc::new(RefCell::new(WorkspaceReconcileWaiters::default()));
        let mut requested = gate.get();
        assert!(requested.request_pass());
        gate.set(requested);
        let old = PendingWorkspaceReconcile::new(&current, &gate, &waiters);
        let successor = PendingWorkspaceReconcile::new(&current, &gate, &waiters);
        drop(old);
        assert!(current.borrow().is_some());
        assert_eq!(gate.get(), requested);
        assert_eq!(successor.finish(None), Some(false));
        assert_eq!(gate.get(), WorkspaceReconcileGate::default());

        let mut requested = gate.get();
        assert!(requested.request_pass());
        gate.set(requested);
        let never_polled = PendingWorkspaceReconcile::new(&current, &gate, &waiters);
        drop(async move { never_polled.finish(None) });
        assert_eq!(gate.get(), WorkspaceReconcileGate::default());
        assert!(current.borrow().is_none());
    }

    #[test]
    fn pending_native_window_creation_releases_failed_and_cancelled_attempts() {
        use super::PendingWindowCreation;
        use std::cell::RefCell;
        use std::collections::HashMap;
        use std::future::Future;
        use std::rc::Rc;
        use std::task::{Context, Poll};

        let pending = Rc::new(RefCell::new(HashMap::new()));
        let cancelled = PendingWindowCreation::try_begin(&pending, 7).unwrap();
        let other = PendingWindowCreation::try_begin(&pending, 8).unwrap();
        assert!(PendingWindowCreation::try_begin(&pending, 7).is_none());
        let mut creation = Box::pin(async move {
            let _attempt = cancelled;
            std::future::pending::<()>().await;
        });
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(creation.as_mut().poll(&mut cx), Poll::Pending);
        drop(creation);
        assert!(!pending.borrow().contains_key(&7));
        assert!(pending.borrow().contains_key(&8));

        let retry = PendingWindowCreation::try_begin(&pending, 7).unwrap();
        let mut failed_creation = Box::pin(async move {
            let _attempt = retry;
            Err::<(), _>(anyhow::anyhow!("native renderer initialization failed"))
        });
        assert!(matches!(
            failed_creation.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        assert!(!pending.borrow().contains_key(&7));
        let success = PendingWindowCreation::try_begin(&pending, 7).unwrap();
        drop(success);
        drop(other);
        assert!(pending.borrow().is_empty());
    }

    #[test]
    fn pending_native_window_creation_cannot_release_a_replacement_attempt() {
        use super::PendingWindowCreation;
        use std::cell::RefCell;
        use std::collections::HashMap;
        use std::rc::Rc;

        let pending = Rc::new(RefCell::new(HashMap::new()));
        let old = PendingWindowCreation::try_begin(&pending, 7).unwrap();
        // Workspace reconciliation can retire the old view while its native
        // initialization is still awaiting completion.
        pending.borrow_mut().remove(&7);
        let current = PendingWindowCreation::try_begin(&pending, 7).unwrap();
        drop(old);
        assert!(PendingWindowCreation::try_begin(&pending, 7).is_none());
        drop(current);
        assert!(PendingWindowCreation::try_begin(&pending, 7).is_some());
    }

    #[test]
    fn background_frontend_dispatch_constructs_and_polls_local_future_on_owner() {
        use promise::spawn::{MainThreadAdmissionLimits, MainThreadServiceClass, SimpleExecutor};
        use std::rc::Rc;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // Exercise the actual frontend dispatcher without initializing a
        // frontend, native window, or event loop. A single available slot also
        // proves that the handoff retains its original admission.
        let executor =
            SimpleExecutor::try_with_limits(MainThreadAdmissionLimits::new(1, 4096, 0, 0).unwrap())
                .unwrap();
        let owner = std::thread::current().id();
        let constructed = Arc::new(AtomicBool::new(false));
        let polled = Arc::new(AtomicBool::new(false));
        let constructed_in_factory = Arc::clone(&constructed);
        let polled_in_future = Arc::clone(&polled);
        std::thread::spawn(move || {
            assert_ne!(std::thread::current().id(), owner);
            super::schedule_frontend_main_thread(
                MainThreadServiceClass::Background,
                4096,
                "background frontend regression",
                move || {
                    assert_eq!(std::thread::current().id(), owner);
                    constructed_in_factory.store(true, Ordering::Release);
                    let local = Rc::new(owner);
                    async move {
                        assert_eq!(std::thread::current().id(), *local);
                        polled_in_future.store(true, Ordering::Release);
                    }
                },
            );
        })
        .join()
        .expect("background notification must only transfer its factory");

        assert!(!constructed.load(Ordering::Acquire));
        assert!(!polled.load(Ordering::Acquire));
        assert_eq!(executor.admission_snapshot().active_tasks, 1);
        assert!(executor.try_tick().unwrap());
        assert!(constructed.load(Ordering::Acquire));
        assert!(!polled.load(Ordering::Acquire));
        assert_eq!(executor.admission_snapshot().active_tasks, 1);
        assert!(executor.try_tick().unwrap());
        assert!(polled.load(Ordering::Acquire));
        assert_eq!(executor.admission_snapshot().active_tasks, 0);
        assert_eq!(executor.queue_snapshot().depth, 0);
    }

    #[test]
    fn osc22_request_parser_accepts_terminal_cursor_slugs() {
        assert_eq!(
            cursor_shape_slug_from_osc22_request("block-blinking"),
            Some(CursorShapeSlug::BlockBlinking)
        );
        assert_eq!(
            cursor_shape_slug_from_osc22_request("steady underline"),
            Some(CursorShapeSlug::UnderlineSteady)
        );
        assert_eq!(
            cursor_shape_slug_from_osc22_request("bar_steady"),
            Some(CursorShapeSlug::BarSteady)
        );
    }

    #[test]
    fn osc22_request_parser_accepts_common_text_aliases() {
        for alias in ["text", "beam", "ibeam"] {
            assert_eq!(
                cursor_shape_slug_from_osc22_request(alias),
                Some(CursorShapeSlug::BarBlinking),
                "alias={alias}",
            );
        }
    }

    #[test]
    fn osc22_request_parser_rejects_unsupported_css_shapes() {
        assert_eq!(cursor_shape_slug_from_osc22_request("wait"), None);
        assert_eq!(cursor_shape_slug_from_osc22_request("crosshair"), None);
        assert_eq!(cursor_shape_slug_from_osc22_request("not-a-shape"), None);
    }

    #[test]
    fn osc22_slug_maps_to_native_mouse_cursor() {
        assert_eq!(
            mouse_cursor_for_osc22_shape(CursorShapeSlug::Default),
            MouseCursor::Arrow
        );
        assert_eq!(
            mouse_cursor_for_osc22_shape(CursorShapeSlug::BlockSteady),
            MouseCursor::Arrow
        );
        assert_eq!(
            mouse_cursor_for_osc22_shape(CursorShapeSlug::UnderlineBlinking),
            MouseCursor::Text
        );
        assert_eq!(
            mouse_cursor_for_osc22_shape(CursorShapeSlug::BarSteady),
            MouseCursor::Text
        );
    }

    #[test]
    fn osc22_accessibility_announcement_names_shape() {
        assert_eq!(
            osc22_accessibility_announcement(CursorShapeSlug::UnderlineSteady),
            "Cursor shape underline steady"
        );
    }

    #[test]
    fn terminal_toast_focus_flag_controls_activation_payload() {
        assert!(terminal_toast_action(false, 42).is_none());

        let action = terminal_toast_action(true, 42).expect("focus=true should attach action");
        assert_eq!(action.label(), "Focus");
    }
}

thread_local! {
    static FRONT_END: RefCell<Option<Rc<GuiFrontEnd>>> = RefCell::new(None);
}

pub fn try_front_end() -> Option<Rc<GuiFrontEnd>> {
    FRONT_END.with(|f| f.borrow().as_ref().map(Rc::clone))
}

pub struct WorkspaceSwitcher {
    new_name: String,
}

impl WorkspaceSwitcher {
    pub fn new(new_name: &str) -> Option<Self> {
        let front_end = try_front_end()?;
        *front_end.switching_workspaces.borrow_mut() = true;
        Some(Self {
            new_name: new_name.to_string(),
        })
    }

    pub fn do_switch(self) {
        // Drop is invoked, which will complete the switch
    }
}

impl Drop for WorkspaceSwitcher {
    fn drop(&mut self) {
        if let Some(front_end) = try_front_end() {
            front_end.switch_workspace(&self.new_name);
        }
    }
}

pub fn shutdown() {
    FRONT_END.with(|f| drop(f.borrow_mut().take()));
}

pub fn try_new() -> Result<Rc<GuiFrontEnd>, Error> {
    let front_end = GuiFrontEnd::try_new()?;
    FRONT_END.with(|f| *f.borrow_mut() = Some(Rc::clone(&front_end)));

    let config_subscription = config::subscribe_to_config_reload({
        move || {
            schedule_frontend_main_thread(
                MainThreadServiceClass::Background,
                FRONTEND_MAIN_THREAD_ESTIMATED_BYTES,
                "menu rebuild after configuration reload",
                || async {
                    crate::commands::CommandDef::recreate_menubar(&config::configuration());
                },
            );
            true
        }
    });
    front_end
        .config_subscription
        .borrow_mut()
        .replace(config_subscription);

    Ok(front_end)
}
