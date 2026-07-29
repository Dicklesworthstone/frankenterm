use crate::client::{Client, RpcGenerationScope};
use crate::pane::ClientPane;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use codec::{ListPanesResponse, SpawnV2, SplitPane};
use config::keyassignment::SpawnTabDomain;
use config::{SshDomain, TlsDomainClient, UnixDomain};
use mux::client::ClientId;
use mux::connui::{ConnectionUI, ConnectionUIParams};
use mux::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use mux::pane::{reserve_pane_ids, Pane, PaneId};
use mux::tab::{PaneNode, SplitRequest, Tab, TabId};
use mux::window::WindowId;
use mux::{CurrentPane, Mux, MuxNotification};
use portable_pty::CommandBuilder;
use promise::spawn::spawn_into_new_thread;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use wezterm_term::TerminalSize;

pub struct ClientInner {
    pub client: Client,
    pub local_domain_id: DomainId,
    owner_client_id: Option<Arc<ClientId>>,
    pub local_echo_threshold_ms: Option<u64>,
    pub overlay_lag_indicator: bool,
    remote_to_local_window: Mutex<HashMap<WindowId, WindowId>>,
    remote_to_local_tab: Mutex<HashMap<TabId, TabId>>,
    remote_to_local_pane: Mutex<HashMap<PaneId, PaneId>>,
    spare_local_pane_ids: Mutex<Vec<PaneId>>,
    pub focused_remote_pane_id: Mutex<Option<PaneId>>,
    detached: AtomicBool,
    topology_request_epoch: AtomicU64,
}

pub(crate) fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, label: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("recovering poisoned {label} lock");
            poisoned.into_inner()
        }
    }
}

fn collect_remote_pane_ids(
    node: &PaneNode,
    expected_tree_identity: &mut Option<(WindowId, TabId)>,
    seen_pane_ids: &mut HashSet<PaneId>,
    pane_ids: &mut Vec<PaneId>,
) -> anyhow::Result<()> {
    match node {
        PaneNode::Empty => {}
        PaneNode::Split { left, right, .. } => {
            collect_remote_pane_ids(left, expected_tree_identity, seen_pane_ids, pane_ids)?;
            collect_remote_pane_ids(right, expected_tree_identity, seen_pane_ids, pane_ids)?;
        }
        PaneNode::Leaf(entry) => {
            let identity = (entry.window_id, entry.tab_id);
            if expected_tree_identity.is_some_and(|expected| expected != identity) {
                bail!(
                    "malformed ListPanes response: one tab tree mixes window/tab identities {:?} \
                     and {:?}",
                    expected_tree_identity,
                    identity
                );
            }
            *expected_tree_identity = Some(identity);
            if !seen_pane_ids.insert(entry.pane_id) {
                bail!(
                    "malformed ListPanes response: remote pane {} appears more than once",
                    entry.pane_id
                );
            }
            pane_ids.push(entry.pane_id);
        }
    }
    Ok(())
}

fn index_live_client_pane(
    by_remote_pane: &mut HashMap<PaneId, PaneId>,
    remote_pane_id: PaneId,
    local_pane_id: PaneId,
) -> anyhow::Result<()> {
    match by_remote_pane.entry(remote_pane_id) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(local_pane_id);
            Ok(())
        }
        std::collections::hash_map::Entry::Occupied(entry) if *entry.get() == local_pane_id => {
            Ok(())
        }
        std::collections::hash_map::Entry::Occupied(entry) => {
            let existing_local_pane_id = *entry.get();
            bail!(
                "inconsistent live client topology: remote pane {remote_pane_id} is mirrored by \
                 local panes {existing_local_pane_id} and {local_pane_id}"
            )
        }
    }
}

struct LocalPaneIdReservations<'a> {
    spare_pool: &'a Mutex<Vec<PaneId>>,
    by_remote_pane: HashMap<PaneId, PaneId>,
}

impl LocalPaneIdReservations<'_> {
    fn take(&mut self, remote_pane_id: PaneId) -> Option<PaneId> {
        self.by_remote_pane.remove(&remote_pane_id)
    }
}

impl Drop for LocalPaneIdReservations<'_> {
    fn drop(&mut self) {
        if self.by_remote_pane.is_empty() {
            return;
        }
        let mut spare_pool = lock_or_recover(self.spare_pool, "spare_local_pane_ids");
        spare_pool.extend(
            self.by_remote_pane
                .drain()
                .map(|(_, local_pane_id)| local_pane_id),
        );
    }
}

impl ClientInner {
    fn remote_to_local_window(&self, remote_window_id: WindowId) -> Option<WindowId> {
        let map = lock_or_recover(&self.remote_to_local_window, "remote_to_local_window");
        map.get(&remote_window_id).cloned()
    }

    pub(crate) fn expire_stale_mappings(&self, current: &CurrentPane<'_>) {
        self.remote_to_local_pane
            .lock()
            .unwrap_or_else(|poisoned| {
                log::warn!("recovering poisoned remote_to_local_pane lock");
                poisoned.into_inner()
            })
            .retain(|_remote_pane_id, local_pane_id| current.contains_pane_id(*local_pane_id));

        self.remote_to_local_tab
            .lock()
            .unwrap_or_else(|poisoned| {
                log::warn!("recovering poisoned remote_to_local_tab lock");
                poisoned.into_inner()
            })
            .retain(|remote_tab_id, local_tab_id| {
                if current.tab_has_panes_in_domain(*local_tab_id, self.local_domain_id) {
                    true
                } else {
                    log::trace!(
                        "expire_stale_mappings: domain: {}. will remove \
                            {remote_tab_id} -> {local_tab_id} tab mapping \
                            because tab contains no panes from this domain",
                        self.local_domain_id,
                    );
                    false
                }
            });

        self.remote_to_local_window
            .lock()
            .unwrap_or_else(|poisoned| {
                log::warn!("recovering poisoned remote_to_local_window lock");
                poisoned.into_inner()
            })
            .retain(|_remote_window_id, local_window_id| {
                current.window_has_panes_in_domain(*local_window_id, self.local_domain_id)
            });
    }

    fn record_remote_to_local_window_mapping(
        &self,
        remote_window_id: WindowId,
        local_window_id: WindowId,
    ) {
        let mut map = lock_or_recover(&self.remote_to_local_window, "remote_to_local_window");
        map.insert(remote_window_id, local_window_id);
        log::trace!(
            "record_remote_to_local_window_mapping: {} -> {}",
            remote_window_id,
            local_window_id
        );
    }

    fn local_to_remote_tab(&self, local_tab_id: TabId) -> Option<TabId> {
        let map = lock_or_recover(&self.remote_to_local_tab, "remote_to_local_tab");
        for (remote, local) in map.iter() {
            if *local == local_tab_id {
                return Some(*remote);
            }
        }
        None
    }

    fn local_to_remote_window(&self, local_window_id: WindowId) -> Option<WindowId> {
        let map = lock_or_recover(&self.remote_to_local_window, "remote_to_local_window");
        for (remote, local) in map.iter() {
            if *local == local_window_id {
                return Some(*remote);
            }
        }
        None
    }

    pub fn remote_to_local_pane_id(&self, mux: &Mux, remote_pane_id: PaneId) -> Option<PaneId> {
        let mut pane_map = lock_or_recover(&self.remote_to_local_pane, "remote_to_local_pane");

        if let Some(id) = pane_map.get(&remote_pane_id).copied() {
            let mapping_is_current = mux.get_pane(id).is_some_and(|pane| {
                pane.downcast_ref::<ClientPane>()
                    .is_some_and(|client_pane| {
                        client_pane.belongs_to_client(self)
                            && client_pane.remote_pane_id() == remote_pane_id
                    })
            });
            if mapping_is_current {
                return Some(id);
            }
            pane_map.remove(&remote_pane_id);
        }

        for pane in mux.iter_panes() {
            if pane.domain_id() != self.local_domain_id {
                continue;
            }
            if let Some(pane) = pane.downcast_ref::<ClientPane>() {
                if pane.belongs_to_client(self) && pane.remote_pane_id() == remote_pane_id {
                    let local_pane_id = pane.pane_id();
                    pane_map.insert(remote_pane_id, local_pane_id);
                    return Some(local_pane_id);
                }
            }
        }
        None
    }
    pub fn remove_old_pane_mapping(&self, remote_pane_id: PaneId) {
        let mut pane_map = lock_or_recover(&self.remote_to_local_pane, "remote_to_local_pane");
        pane_map.remove(&remote_pane_id);
    }

    fn record_remote_to_local_pane_mapping(&self, remote_pane_id: PaneId, local_pane_id: PaneId) {
        let mut pane_map = lock_or_recover(&self.remote_to_local_pane, "remote_to_local_pane");
        pane_map.insert(remote_pane_id, local_pane_id);
    }

    fn reserve_local_pane_ids(
        &self,
        remote_pane_ids: Vec<PaneId>,
    ) -> Result<LocalPaneIdReservations<'_>, mux::IdAllocationError> {
        let mut spare_pool = lock_or_recover(&self.spare_local_pane_ids, "spare_local_pane_ids");
        let additional = remote_pane_ids.len().saturating_sub(spare_pool.len());
        if additional > 0 {
            spare_pool.extend(reserve_pane_ids(additional)?);
        }
        let first_reserved = spare_pool.len() - remote_pane_ids.len();
        let local_pane_ids = spare_pool.split_off(first_reserved);
        drop(spare_pool);

        Ok(LocalPaneIdReservations {
            spare_pool: &self.spare_local_pane_ids,
            by_remote_pane: remote_pane_ids.into_iter().zip(local_pane_ids).collect(),
        })
    }

    pub fn remove_old_tab_mapping(&self, remote_tab_id: TabId) {
        let mut tab_map = lock_or_recover(&self.remote_to_local_tab, "remote_to_local_tab");
        let old = tab_map.remove(&remote_tab_id);
        log::trace!("remove_old_tab_mapping: {remote_tab_id} -> {old:?}");
    }

    fn record_remote_to_local_tab_mapping(&self, remote_tab_id: TabId, local_tab_id: TabId) {
        let mut map = lock_or_recover(&self.remote_to_local_tab, "remote_to_local_tab");
        let prior = map.insert(remote_tab_id, local_tab_id);
        log::trace!(
            "record_remote_to_local_tab_mapping: {} -> {} \
             (prior={prior:?}, domain={})",
            remote_tab_id,
            local_tab_id,
            self.local_domain_id,
        );
    }

    pub fn remote_to_local_tab_id(&self, remote_tab_id: TabId) -> Option<TabId> {
        let map = lock_or_recover(&self.remote_to_local_tab, "remote_to_local_tab");
        map.get(&remote_tab_id).copied()
    }

    pub fn is_local(&self) -> bool {
        self.client.is_local
    }
}

#[derive(Clone, Debug)]
pub enum ClientDomainConfig {
    Unix(UnixDomain),
    Tls(TlsDomainClient),
    Ssh(SshDomain),
}

impl ClientDomainConfig {
    pub fn name(&self) -> &str {
        match self {
            ClientDomainConfig::Unix(unix) => &unix.name,
            ClientDomainConfig::Tls(tls) => &tls.name,
            ClientDomainConfig::Ssh(ssh) => &ssh.name,
        }
    }

    pub fn local_echo_threshold_ms(&self) -> Option<u64> {
        match self {
            ClientDomainConfig::Unix(unix) => unix.local_echo_threshold_ms,
            ClientDomainConfig::Tls(tls) => tls.local_echo_threshold_ms,
            ClientDomainConfig::Ssh(ssh) => ssh.local_echo_threshold_ms,
        }
    }

    pub fn overlay_lag_indicator(&self) -> bool {
        match self {
            ClientDomainConfig::Unix(unix) => unix.overlay_lag_indicator,
            ClientDomainConfig::Tls(tls) => tls.overlay_lag_indicator,
            ClientDomainConfig::Ssh(ssh) => ssh.overlay_lag_indicator,
        }
    }

    pub fn label(&self) -> String {
        match self {
            ClientDomainConfig::Unix(unix) => format!("unix mux {}", unix.socket_path().display()),
            ClientDomainConfig::Tls(tls) => format!("TLS mux {}", tls.remote_address),
            ClientDomainConfig::Ssh(ssh) => {
                if let Some(user) = &ssh.username {
                    format!("SSH mux {}@{}", user, ssh.remote_address)
                } else {
                    format!("SSH mux {}", ssh.remote_address)
                }
            }
        }
    }

    pub fn connect_automatically(&self) -> bool {
        match self {
            ClientDomainConfig::Unix(unix) => unix.connect_automatically,
            ClientDomainConfig::Tls(tls) => tls.connect_automatically,
            ClientDomainConfig::Ssh(ssh) => ssh.connect_automatically,
        }
    }
}

impl ClientInner {
    pub fn new(
        local_domain_id: DomainId,
        client: Client,
        owner_client_id: Option<Arc<ClientId>>,
        local_echo_threshold_ms: Option<u64>,
        overlay_lag_indicator: bool,
    ) -> Self {
        Self {
            client,
            local_domain_id,
            owner_client_id,
            local_echo_threshold_ms,
            overlay_lag_indicator,
            remote_to_local_window: Mutex::new(HashMap::new()),
            remote_to_local_tab: Mutex::new(HashMap::new()),
            remote_to_local_pane: Mutex::new(HashMap::new()),
            spare_local_pane_ids: Mutex::new(Vec::new()),
            focused_remote_pane_id: Mutex::new(None),
            detached: AtomicBool::new(false),
            topology_request_epoch: AtomicU64::new(0),
        }
    }

    pub(crate) fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    fn mark_detached(&self) {
        self.detached.store(true, Ordering::Release);
    }

    fn begin_topology_request(&self) -> anyhow::Result<u64> {
        self.topology_request_epoch
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(|prior| prior + 1)
            .map_err(|_| anyhow!("client topology request epoch exhausted"))
    }

    fn topology_request_is_current(&self, epoch: u64) -> bool {
        self.topology_request_epoch.load(Ordering::Acquire) == epoch
    }
}

pub struct ClientDomain {
    config: ClientDomainConfig,
    label: String,
    inner: Mutex<Option<Arc<ClientInner>>>,
    initial_attachment_pending: AtomicBool,
    retired: AtomicBool,
    local_domain_id: DomainId,
    mux_owner: Weak<Mux>,
    mux_subscriber_id: Option<usize>,
}

struct InitialAttachmentClaim<'a> {
    pending: &'a AtomicBool,
}

impl Drop for InitialAttachmentClaim<'_> {
    fn drop(&mut self) {
        self.pending.store(false, Ordering::Release);
    }
}

impl Drop for ClientDomain {
    fn drop(&mut self) {
        if let (Some(mux), Some(subscriber_id)) = (self.mux_owner.upgrade(), self.mux_subscriber_id)
        {
            mux.unsubscribe(subscriber_id);
        }
    }
}

async fn update_remote_workspace(
    inner: Arc<ClientInner>,
    pdu: codec::SetWindowWorkspace,
) -> anyhow::Result<()> {
    if inner.is_detached() {
        return Ok(());
    }
    inner.client.set_window_workspace(pdu).await?;
    Ok(())
}

fn active_workspace_sync_request(
    owner_client_id: Option<&Arc<ClientId>>,
    changed_client_id: &Arc<ClientId>,
    mux: &Mux,
) -> Option<codec::SetActiveWorkspace> {
    let owner_client_id = owner_client_id?;
    if !Arc::ptr_eq(owner_client_id, changed_client_id)
        || !mux.client_registration_is_current(owner_client_id)
    {
        return None;
    }

    Some(codec::SetActiveWorkspace {
        workspace: mux.active_workspace_for_client(changed_client_id),
    })
}

fn current_active_workspace_sync(
    inner: &ClientInner,
    mux: &Mux,
) -> Option<codec::SetActiveWorkspace> {
    let owner_client_id = inner.owner_client_id.as_ref()?;
    if !mux.client_registration_is_current(owner_client_id) {
        return None;
    }
    Some(codec::SetActiveWorkspace {
        workspace: mux.active_workspace_for_client(owner_client_id),
    })
}

fn workspace_for_spawn_window(mux: &Mux, window_id: WindowId) -> String {
    mux.get_window(window_id)
        .map(|window| window.get_workspace().to_string())
        .unwrap_or_else(|| mux.active_workspace())
}

fn client_inner_is_current(mux: &Mux, domain: &Arc<dyn Domain>, inner: &Arc<ClientInner>) -> bool {
    !inner.is_detached()
        && mux
            .get_domain(domain.domain_id())
            .is_some_and(|current| Arc::ptr_eq(&current, domain))
        && domain
            .downcast_ref::<ClientDomain>()
            .is_some_and(|client_domain| client_domain.inner_is_current(inner))
}

fn mux_notify_client_domain(
    owner: &Weak<Mux>,
    local_domain_id: DomainId,
    notif: MuxNotification,
) -> bool {
    let Some(mux) = owner.upgrade() else {
        return false;
    };
    let domain = match mux.get_domain(local_domain_id) {
        Some(domain) => domain,
        // ClientDomain::new installs the subscriber before the caller can
        // publish the domain. Keep that short pre-registration interval alive;
        // the ClientDomain Drop guard unsubscribes if publication never occurs
        // or after exact domain retirement.
        None => return true,
    };
    let client_domain = match domain.downcast_ref::<ClientDomain>() {
        Some(c) => c,
        None => return false,
    };

    match notif {
        MuxNotification::ActiveWorkspaceChanged(client_id) => {
            if let Some(inner) = client_domain.inner() {
                if let Some(request) =
                    active_workspace_sync_request(inner.owner_client_id.as_ref(), &client_id, &mux)
                {
                    let rpc = inner.client.set_active_workspace(request);
                    let mux = Arc::clone(&mux);
                    let domain = Arc::clone(&domain);
                    promise::spawn::spawn(async move {
                        if !client_inner_is_current(&mux, &domain, &inner) {
                            return Ok(());
                        }
                        let _ = rpc.await;
                        anyhow::Result::<()>::Ok(())
                    })
                    .detach();
                }
            }
        }
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => {
            if let Some(inner) = client_domain.inner() {
                let workspaces = mux.iter_workspaces();
                if workspaces.contains(&old_workspace) {
                    let rpc = inner.client.rename_workspace(codec::RenameWorkspace {
                        old_workspace,
                        new_workspace,
                    });
                    let mux = Arc::clone(&mux);
                    let domain = Arc::clone(&domain);
                    promise::spawn::spawn(async move {
                        if !client_inner_is_current(&mux, &domain, &inner) {
                            return Ok(());
                        }
                        rpc.await?;
                        anyhow::Result::<()>::Ok(())
                    })
                    .detach();
                }
            }
        }
        MuxNotification::WindowWorkspaceChanged(window_id) => {
            // Mux::get_window() may trigger a borrow error if called
            // immediately; defer the bulk of this work.
            // <https://github.com/wezterm/wezterm/issues/2638>
            let mux = Arc::clone(&mux);
            let domain = Arc::clone(&domain);
            promise::spawn::spawn_into_main_thread(async move {
                if !mux
                    .get_domain(local_domain_id)
                    .is_some_and(|current| Arc::ptr_eq(&current, &domain))
                {
                    return;
                }
                let client_domain = match domain.downcast_ref::<ClientDomain>() {
                    Some(domain) => domain,
                    None => return,
                };
                if let Some(remote_window_id) = client_domain.local_to_remote_window_id(window_id) {
                    if let Some(workspace) = mux
                        .get_window(window_id)
                        .map(|w| w.get_workspace().to_string())
                    {
                        let Some(inner) = client_domain.inner() else {
                            return;
                        };
                        if !client_inner_is_current(&mux, &domain, &inner) {
                            return;
                        }
                        let request = codec::SetWindowWorkspace {
                            window_id: remote_window_id,
                            workspace,
                        };
                        let _ = update_remote_workspace(inner, request).await;
                    }
                } else {
                    log::debug!(
                        "local window id {window_id} has no known remote window \
                        id while reconciling a local WindowWorkspaceChanged event"
                    );
                }
            })
            .detach();
        }
        MuxNotification::TabTitleChanged { tab_id, title } => {
            if let Some(remote_tab_id) = client_domain.local_to_remote_tab_id(tab_id) {
                if let Some(inner) = client_domain.inner() {
                    let rpc = inner.client.set_tab_title(codec::TabTitleChanged {
                        tab_id: remote_tab_id,
                        title,
                    });
                    let mux = Arc::clone(&mux);
                    let domain = Arc::clone(&domain);
                    promise::spawn::spawn(async move {
                        if !client_inner_is_current(&mux, &domain, &inner) {
                            return Ok(());
                        }
                        rpc.await?;
                        anyhow::Result::<()>::Ok(())
                    })
                    .detach();
                }
            }
        }
        MuxNotification::WindowTitleChanged {
            window_id,
            title: _,
        } => {
            if let Some(inner) = client_domain.inner() {
                let mux = Arc::clone(&mux);
                let domain = Arc::clone(&domain);
                promise::spawn::spawn_into_main_thread(async move {
                    // De-bounce the title propagation.
                    // There is a bit of a race condition with these async
                    // updates that can trigger a cycle of WindowTitleChanged
                    // PDUs being exchanged between client and server if the
                    // title is changed twice in quick succession.
                    // To avoid that, here on the client, we wait a second
                    // and then report the now-current name of the window, rather
                    // than propagating the title encoded in the MuxNotification.
                    promise::spawn::sleep(std::time::Duration::from_secs(1)).await;
                    if !client_inner_is_current(&mux, &domain, &inner) {
                        return Ok(());
                    }
                    let Some(client_domain) = domain.downcast_ref::<ClientDomain>() else {
                        return Ok(());
                    };
                    let Some(remote_window_id) =
                        client_domain.local_to_remote_window_id(window_id)
                    else {
                        return Ok(());
                    };
                    let title = mux
                        .get_window(window_id)
                        .map(|win| win.get_title().to_string());
                    if let Some(title) = title {
                        inner
                            .client
                            .set_window_title(codec::WindowTitleChanged {
                                window_id: remote_window_id,
                                title,
                            })
                            .await?;
                    }
                    anyhow::Result::<()>::Ok(())
                })
                .detach();
            }
        }
        _ => {}
    }
    true
}

impl ClientDomain {
    pub fn new(config: ClientDomainConfig, mux_owner: &Arc<Mux>) -> anyhow::Result<Self> {
        let local_domain_id = alloc_domain_id();
        let label = config.label();
        let owner = Arc::downgrade(mux_owner);
        let mux_subscriber_id = mux_owner
            .subscribe(move |notif| mux_notify_client_domain(&owner, local_domain_id, notif))
            .context("allocate client-domain mux subscription")?;
        Ok(Self {
            config,
            label,
            inner: Mutex::new(None),
            initial_attachment_pending: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            local_domain_id,
            mux_owner: Arc::downgrade(mux_owner),
            mux_subscriber_id: Some(mux_subscriber_id),
        })
    }

    pub(crate) fn inner(&self) -> Option<Arc<ClientInner>> {
        lock_or_recover(&self.inner, "client_domain_inner")
            .as_ref()
            .map(Arc::clone)
    }

    fn ensure_mux_owner(&self, mux: &Arc<Mux>) -> anyhow::Result<()> {
        let owner = self
            .mux_owner
            .upgrade()
            .context("client domain's owning mux is not available")?;
        if !Arc::ptr_eq(&owner, mux) {
            bail!(
                "client domain {} cannot operate on a different mux instance",
                self.local_domain_id
            );
        }
        Ok(())
    }

    pub(crate) fn inner_is_current(&self, expected: &Arc<ClientInner>) -> bool {
        !self.retired.load(Ordering::Acquire)
            && lock_or_recover(&self.inner, "client_domain_inner")
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, expected))
    }

    pub fn connect_automatically(&self) -> bool {
        self.config.connect_automatically()
    }

    pub fn perform_detach(&self) {
        let expected = self.inner();
        if let Some(expected) = expected {
            let _ = self.perform_detach_if_current(&expected);
        } else {
            self.retired.store(true, Ordering::Release);
            let _ = self.remove_exact_domain_registration();
        }
    }

    /// Retire only the exact attachment observed by the caller.
    ///
    /// The compare-and-take happens under the attachment slot lock. Teardown
    /// then passes the exact registered trait object back to the mux rather
    /// than manufacturing a new trait-object view from `self`.
    pub(crate) fn perform_detach_if_current(&self, expected: &Arc<ClientInner>) -> bool {
        let retired = {
            let mut inner = lock_or_recover(&self.inner, "client_domain_inner");
            if !inner
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, expected))
            {
                return false;
            }
            self.retired.store(true, Ordering::Release);
            let retired = inner
                .take()
                .expect("exact client attachment disappeared while its lock was held");
            retired.mark_detached();
            retired
        };
        drop(retired);

        log::info!(
            "detached exact attachment for domain {}",
            self.local_domain_id
        );
        let _ = self.remove_exact_domain_registration();
        true
    }

    fn remove_exact_domain_registration(&self) -> bool {
        let Some(mux) = self.mux_owner.upgrade() else {
            return false;
        };
        let Some(registered) = mux.get_domain(self.local_domain_id) else {
            return false;
        };
        if !registered
            .downcast_ref::<Self>()
            .is_some_and(|current| std::ptr::eq(current, self))
        {
            return false;
        }
        mux.domain_was_detached_if_same(&registered)
    }

    pub(crate) fn remote_to_local_window_id(&self, remote_window_id: WindowId) -> Option<WindowId> {
        let inner = self.inner()?;
        inner.remote_to_local_window(remote_window_id)
    }

    pub(crate) fn local_to_remote_window_id(&self, local_window_id: WindowId) -> Option<WindowId> {
        let inner = self.inner()?;
        inner.local_to_remote_window(local_window_id)
    }

    pub(crate) fn local_to_remote_tab_id(&self, local_tab_id: TabId) -> Option<TabId> {
        let inner = self.inner()?;
        inner.local_to_remote_tab(local_tab_id)
    }

    /// The reader in the mux may have decided to give up on one or
    /// more tabs at the time that a disconnect was detected, and
    /// it's also possible that another client connected and adjusted
    /// the set of tabs since we were connected, so we need to re-sync.
    pub(crate) async fn reattach_if_current(
        mux: Arc<Mux>,
        domain: Arc<dyn Domain>,
        expected: Arc<ClientInner>,
        rpc: RpcGenerationScope,
        ui: ConnectionUI,
    ) -> anyhow::Result<()> {
        let domain_id = domain.domain_id();
        let current = mux
            .get_domain(domain_id)
            .is_some_and(|candidate| Arc::ptr_eq(&candidate, &domain));
        if !current {
            return Ok(());
        }
        let client_domain = domain
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", domain_id))?;
        if !client_domain.inner_is_current(&expected) {
            return Ok(());
        }

        // Every physical server connection owns a fresh SessionHandler with no
        // client identity. Re-establish codec compatibility and SetClientId on
        // the exact successor generation before any topology or workspace RPC.
        expected
            .client
            .verify_version_compat_with_scope(&ui, &rpc)
            .await?;

        if !Self::sync_remote_topology(
            Arc::clone(&mux),
            client_domain,
            Arc::clone(&expected),
            &rpc,
            None,
        )
        .await?
        {
            return Ok(());
        }

        if !client_inner_is_current(&mux, &domain, &expected) {
            return Ok(());
        }
        if let Some(request) = current_active_workspace_sync(&expected, &mux) {
            let _ = rpc.set_active_workspace(request).await;
        }
        if !client_inner_is_current(&mux, &domain, &expected) {
            return Ok(());
        }

        ui.close();
        Ok(())
    }

    pub(crate) async fn resync_if_current(
        &self,
        mux: Arc<Mux>,
        expected: Arc<ClientInner>,
        rpc: &RpcGenerationScope,
    ) -> anyhow::Result<()> {
        if self.inner_is_current(&expected) {
            let _ = Self::sync_remote_topology(mux, self, expected, rpc, None).await?;
        }
        Ok(())
    }

    async fn sync_remote_topology(
        mux: Arc<Mux>,
        domain: &Self,
        inner: Arc<ClientInner>,
        rpc: &RpcGenerationScope,
        primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<bool> {
        let request_epoch = inner.begin_topology_request()?;
        let incarnation_is_current = || {
            !inner.is_detached()
                && inner.topology_request_is_current(request_epoch)
                && domain.inner_is_current(&inner)
                && mux
                    .get_domain(domain.local_domain_id)
                    .is_some_and(|current| {
                        current
                            .downcast_ref::<Self>()
                            .is_some_and(|current| std::ptr::eq(current, domain))
                    })
        };
        if !incarnation_is_current() {
            return Ok(false);
        }
        let panes = rpc.list_panes().await?;
        if !incarnation_is_current() {
            return Ok(false);
        }
        Self::process_pane_list(&mux, Arc::clone(&inner), panes, primary_window_id)?;
        Ok(incarnation_is_current())
    }

    fn resolve_remote_spawn_entities(
        mux: &Mux,
        inner: &Arc<ClientInner>,
        result: codec::SpawnResponse,
    ) -> anyhow::Result<(Arc<Tab>, Arc<dyn Pane>, WindowId)> {
        if inner.is_detached() {
            bail!("client attachment retired before remote spawn resolution");
        }
        let local_tab_id = inner
            .remote_to_local_tab_id(result.tab_id)
            .ok_or_else(|| anyhow!("remote tab {} didn't resolve after resync", result.tab_id))?;
        let local_pane_id = lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane")
            .get(&result.pane_id)
            .copied()
            .ok_or_else(|| anyhow!("remote pane {} didn't resolve after resync", result.pane_id))?;
        let local_window_id = inner
            .remote_to_local_window(result.window_id)
            .ok_or_else(|| {
                anyhow!(
                    "remote window {} didn't resolve after resync",
                    result.window_id
                )
            })?;

        let tab = mux
            .get_tab(local_tab_id)
            .ok_or_else(|| anyhow!("local tab {local_tab_id} is invalid"))?;
        let pane = mux
            .get_pane(local_pane_id)
            .ok_or_else(|| anyhow!("local pane {local_pane_id} is invalid"))?;
        let client_pane = pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("local pane {local_pane_id} is not a ClientPane"))?;
        if !client_pane.belongs_to_client(inner) || client_pane.remote_pane_id() != result.pane_id {
            bail!(
                "local pane {local_pane_id} does not belong to the current client incarnation for \
                 remote pane {}",
                result.pane_id
            );
        }
        if !tab
            .iter_all_panes()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &pane))
        {
            bail!(
                "local pane {local_pane_id} is not attached to resolved local tab {local_tab_id}"
            );
        }
        if mux.window_containing_tab(local_tab_id) != Some(local_window_id) {
            bail!(
                "resolved local tab {local_tab_id} is not attached to local window \
                 {local_window_id}"
            );
        }

        Ok((tab, pane, local_window_id))
    }

    fn process_pane_list(
        mux: &Arc<Mux>,
        inner: Arc<ClientInner>,
        panes: ListPanesResponse,
        mut primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        if panes.tabs.len() != panes.tab_titles.len() {
            bail!(
                "malformed ListPanes response: {} tab tree(s) but {} tab title(s); refusing \
                 identifier reservation or topology mutation",
                panes.tabs.len(),
                panes.tab_titles.len()
            );
        }
        log::debug!(
            "domain {}: ListPanes result {:#?}",
            inner.local_domain_id,
            panes
        );

        // Check out one fallback local identifier for every unique remote pane
        // before publishing any tabs, panes, or windows. This remains safe if
        // a pane from the live snapshot disappears during the tree walk. IDs
        // that are not consumed are returned to the per-domain spare bank, so
        // stable large-session resyncs do not burn through the process-wide
        // PaneId namespace.
        let mut remote_pane_ids = Vec::new();
        let mut seen_remote_pane_ids = HashSet::new();
        let mut seen_remote_tab_ids = HashSet::new();
        for tabroot in &panes.tabs {
            let mut tree_identity = None;
            collect_remote_pane_ids(
                tabroot,
                &mut tree_identity,
                &mut seen_remote_pane_ids,
                &mut remote_pane_ids,
            )?;
            if let Some((_, tab_id)) = tree_identity {
                if !seen_remote_tab_ids.insert(tab_id) {
                    bail!(
                        "malformed ListPanes response: remote tab {tab_id} appears in more than \
                         one tree"
                    );
                }
            }
        }
        let mut reserved_local_pane_ids = inner
            .reserve_local_pane_ids(remote_pane_ids)
            .context("reserve local pane identifiers for remote topology")?;
        // Resolve the full live ClientPane set once. Calling
        // `remote_to_local_pane_id` for each missing remote pane would scan the
        // mux repeatedly and make a first large-session sync quadratic.
        let live_panes = mux.iter_panes();
        let mut local_pane_ids_by_remote = HashMap::with_capacity(live_panes.len());
        for pane in live_panes {
            if pane.domain_id() != inner.local_domain_id {
                continue;
            }
            if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                if !client_pane.belongs_to_client(&inner) {
                    continue;
                }
                let remote_pane_id = client_pane.remote_pane_id();
                let local_pane_id = pane.pane_id();
                index_live_client_pane(
                    &mut local_pane_ids_by_remote,
                    remote_pane_id,
                    local_pane_id,
                )?;
            }
        }
        {
            let mut pane_map = lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane");
            pane_map.extend(
                local_pane_ids_by_remote
                    .iter()
                    .map(|(remote, local)| (*remote, *local)),
            );
        }

        // "Mark" the current set of known remote ids, so that we can "Sweep"
        // any unreferenced ids at the bottom, garbage collection style
        let mut remote_windows_to_forget: HashSet<WindowId> =
            lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window")
                .keys()
                .copied()
                .collect();
        let mut remote_tabs_to_forget: HashSet<WindowId> =
            lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab")
                .keys()
                .copied()
                .collect();
        let mut remote_panes_to_forget: HashSet<WindowId> =
            lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane")
                .keys()
                .copied()
                .collect();

        for (tabroot, tab_title) in panes.tabs.into_iter().zip(panes.tab_titles.iter()) {
            let root_size = match tabroot.root_size() {
                Some(size) => size,
                None => continue,
            };

            if let Some((remote_window_id, remote_tab_id)) = tabroot.window_and_tab_ids() {
                let tab;

                remote_windows_to_forget.remove(&remote_window_id);
                remote_tabs_to_forget.remove(&remote_tab_id);

                if let Some(tab_id) = inner.remote_to_local_tab_id(remote_tab_id) {
                    match mux.get_tab(tab_id) {
                        Some(t) => tab = t,
                        None => {
                            // We likely decided that we hit EOF on the tab and
                            // removed it from the mux.  Let's add it back, but
                            // with a new id.
                            log::trace!(
                                "we had remote_to_local_tab_id mapping of \
                                 {remote_tab_id} -> {tab_id}, but the local \
                                 tab is not in the mux, make a new tab"
                            );
                            inner.remove_old_tab_mapping(remote_tab_id);
                            tab = Arc::new(Tab::new(&root_size));
                            mux.add_tab_no_panes(&tab)?;
                            inner.record_remote_to_local_tab_mapping(remote_tab_id, tab.tab_id());
                        }
                    };
                } else {
                    tab = Arc::new(Tab::new(&root_size));
                    mux.add_tab_no_panes(&tab)?;
                    inner.record_remote_to_local_tab_mapping(remote_tab_id, tab.tab_id());
                }

                mux.set_tab_title(tab.tab_id(), tab_title);

                log::debug!("domain: {} tree: {:#?}", inner.local_domain_id, tabroot);
                let mut workspace = None;
                tab.sync_with_pane_tree(root_size, tabroot, |entry| {
                    workspace.replace(entry.workspace.clone());
                    remote_panes_to_forget.remove(&entry.pane_id);
                    let pane = if let Some(pane_id) =
                        local_pane_ids_by_remote.get(&entry.pane_id).copied()
                    {
                        match mux.get_pane(pane_id) {
                            Some(pane)
                                if pane.downcast_ref::<ClientPane>().is_some_and(
                                    |client_pane| {
                                        client_pane.belongs_to_client(&inner)
                                            && client_pane.remote_pane_id() == entry.pane_id
                                    },
                                ) =>
                            {
                                pane
                            }
                            Some(_) | None => {
                                // We likely decided that we hit EOF on the tab and
                                // removed it from the mux, or this mapping belongs
                                // to an older client incarnation. Add the exact
                                // current remote pane back with a fresh local id.
                                inner.remove_old_pane_mapping(entry.pane_id);
                                let local_pane_id = reserved_local_pane_ids
                                    .take(entry.pane_id)
                                    .ok_or_else(|| {
                                        anyhow!(
                                            "remote pane {} needs a local identifier, but no \
                                             identifier was reserved",
                                            entry.pane_id
                                        )
                                    })?;
                                let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
                                    &inner,
                                    local_pane_id,
                                    entry.tab_id,
                                    entry.pane_id,
                                    entry.size,
                                    &entry.title,
                                    entry.alt_screen_active,
                                ));
                                mux.add_pane(&pane).with_context(|| {
                                    format!("register remote pane {} in mux", entry.pane_id)
                                })?;
                                inner.record_remote_to_local_pane_mapping(
                                    entry.pane_id,
                                    local_pane_id,
                                );
                                local_pane_ids_by_remote.insert(entry.pane_id, local_pane_id);
                                pane
                            }
                        }
                    } else {
                        let local_pane_id =
                            reserved_local_pane_ids.take(entry.pane_id).ok_or_else(|| {
                                anyhow!(
                                    "remote pane {} needs a local identifier, but no identifier \
                                     was reserved",
                                    entry.pane_id
                                )
                            })?;
                        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
                            &inner,
                            local_pane_id,
                            entry.tab_id,
                            entry.pane_id,
                            entry.size,
                            &entry.title,
                            entry.alt_screen_active,
                        ));
                        log::debug!(
                            "domain: {} attaching to remote pane {:?} -> local pane_id {}",
                            inner.local_domain_id,
                            entry,
                            pane.pane_id()
                        );
                        mux.add_pane(&pane).with_context(|| {
                            format!("register remote pane {} in mux", entry.pane_id)
                        })?;
                        inner.record_remote_to_local_pane_mapping(entry.pane_id, local_pane_id);
                        local_pane_ids_by_remote.insert(entry.pane_id, local_pane_id);
                        pane
                    };
                    if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                        client_pane.sync_remote_listing_state(entry.alt_screen_active);
                    }
                    Ok(pane)
                })?;

                if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                    if let Some(mut window) = mux.get_window_mut(local_window_id) {
                        log::debug!(
                            "domain: {} adding tab to existing local window {}",
                            inner.local_domain_id,
                            local_window_id
                        );
                        if window.idx_by_id(tab.tab_id()).is_none() {
                            window.push(&tab);
                        }
                        continue;
                    }
                    log::debug!(
                        "domain: {} dropping stale remote window mapping {} -> {}",
                        inner.local_domain_id,
                        remote_window_id,
                        local_window_id
                    );
                    lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window")
                        .remove(&remote_window_id);
                }

                if let Some(local_window_id) = primary_window_id {
                    // Verify that the workspace is consistent between the local and remote
                    // windows.
                    //
                    // NB: `Mux::get_window` hands back a read guard over the shared
                    // `windows` RwLock, while `add_tab_to_window` acquires the *write*
                    // lock on that same RwLock. Holding the read guard across that call
                    // self-deadlocks parking_lot's (non-reentrant) RwLock — observed as a
                    // hang on remote-domain attach (main thread parked in
                    // `get_window_mut` -> `lock_exclusive_slow`). Decide what to do while
                    // the read guard is alive, then drop it *before* mutating the mux.
                    enum PrimaryWindow {
                        Reuse,
                        WorkspaceMismatch,
                        Disappeared,
                    }
                    let decision = match mux.get_window(local_window_id) {
                        Some(window) => {
                            if Some(window.get_workspace()) == workspace.as_deref() {
                                PrimaryWindow::Reuse
                            } else {
                                PrimaryWindow::WorkspaceMismatch
                            }
                        }
                        None => PrimaryWindow::Disappeared,
                    };
                    // `window` read guard is dropped here, before any write lock.

                    match decision {
                        PrimaryWindow::Reuse => {
                            // Yes! We can use this window
                            log::debug!(
                                "adding remote window {} as tab to local window {}",
                                remote_window_id,
                                local_window_id
                            );
                            inner.record_remote_to_local_window_mapping(
                                remote_window_id,
                                local_window_id,
                            );
                            mux.add_tab_to_window(&tab, local_window_id)?;
                            primary_window_id.take();
                            continue;
                        }
                        PrimaryWindow::WorkspaceMismatch => {}
                        PrimaryWindow::Disappeared => {
                            log::debug!(
                                "primary local window {} disappeared during remote topology sync",
                                local_window_id
                            );
                            primary_window_id.take();
                        }
                    }
                }
                log::debug!(
                    "making new local window for remote {} in workspace {:?}",
                    remote_window_id,
                    workspace
                );
                let position = None;
                let local_window_id = mux.new_empty_window(workspace.take(), position);
                inner.record_remote_to_local_window_mapping(remote_window_id, *local_window_id);
                mux.add_tab_to_window(&tab, *local_window_id)?;
            }
        }

        for (remote_window_id, window_title) in panes.window_titles {
            if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                if let Some(mut window) = mux.get_window_mut(local_window_id) {
                    window.set_title(&window_title);
                } else {
                    log::debug!(
                        "dropping stale title mapping for remote window {} -> local {}",
                        remote_window_id,
                        local_window_id
                    );
                    lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window")
                        .remove(&remote_window_id);
                }
            }
        }

        // "Sweep" away our mapping for ids that are no longer present in the
        // latest sync
        log::debug!(
            "after sync, remote_windows_to_forget={remote_windows_to_forget:?}, \
                    remote_tabs_to_forget={remote_tabs_to_forget:?}, \
                    remote_panes_to_forget={remote_panes_to_forget:?}"
        );
        if !remote_windows_to_forget.is_empty() {
            let mut windows =
                lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window");
            for w in remote_windows_to_forget {
                windows.remove(&w);
            }
        }
        if !remote_tabs_to_forget.is_empty() {
            let mut tabs = lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab");
            for t in remote_tabs_to_forget {
                tabs.remove(&t);
            }
        }
        if !remote_panes_to_forget.is_empty() {
            let mut panes = lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane");
            for p in remote_panes_to_forget {
                panes.remove(&p);
            }
        }

        Ok(())
    }

    fn finish_attach(
        mux: &Arc<Mux>,
        domain_id: DomainId,
        client: Client,
        panes: ListPanesResponse,
        owner_client_id: Option<Arc<ClientId>>,
        primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        let domain_registration = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("invalid domain id {}", domain_id))?;
        let domain = domain_registration
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", domain_id))?;
        if owner_client_id
            .as_ref()
            .is_some_and(|owner| !mux.client_registration_is_current(owner))
        {
            bail!(
                "client domain {domain_id} owner client registration is no longer current before \
                 attachment preparation"
            );
        }
        let threshold = domain.config.local_echo_threshold_ms();
        let overlay_lag_indicator = domain.config.overlay_lag_indicator();
        let inner = Arc::new(ClientInner::new(
            domain_id,
            client,
            owner_client_id,
            threshold,
            overlay_lag_indicator,
        ));

        domain
            .initial_attachment_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow!("client domain {domain_id} already has an attachment pending"))?;
        let _claim = InitialAttachmentClaim {
            pending: &domain.initial_attachment_pending,
        };
        if domain.retired.load(Ordering::Acquire)
            || !mux
                .get_domain(domain_id)
                .is_some_and(|current| Arc::ptr_eq(&current, &domain_registration))
        {
            inner.mark_detached();
            bail!("client domain {domain_id} retired before attachment publication");
        }
        if lock_or_recover(&domain.inner, "client_domain_inner").is_some() {
            inner.mark_detached();
            bail!("client domain {domain_id} already has a published attachment");
        }

        // Process the pane list BEFORE publishing inner to the domain.
        // This prevents concurrent operations from seeing a partially
        // attached domain with incomplete pane mappings. The pending claim
        // rejects a second initial attachment without holding a callback-
        // reentrant mutex across mux topology mutation.
        Self::process_pane_list(mux, Arc::clone(&inner), panes, primary_window_id)?;

        let mut published = lock_or_recover(&domain.inner, "client_domain_inner");
        if domain.retired.load(Ordering::Acquire)
            || !mux
                .get_domain(domain_id)
                .is_some_and(|current| Arc::ptr_eq(&current, &domain_registration))
        {
            inner.mark_detached();
            bail!("client domain {domain_id} retired during attachment preparation");
        }
        if published.is_some() {
            inner.mark_detached();
            bail!("client domain {domain_id} gained an attachment during preparation");
        }
        if inner
            .owner_client_id
            .as_ref()
            .is_some_and(|owner| !mux.client_registration_is_current(owner))
        {
            inner.mark_detached();
            bail!(
                "client domain {domain_id} owner client registration retired during attachment \
                 preparation"
            );
        }
        *published = Some(Arc::clone(&inner));
        drop(published);

        if let Some(request) = current_active_workspace_sync(&inner, mux) {
            let rpc = inner.client.set_active_workspace(request);
            promise::spawn::spawn(async move {
                let _ = rpc.await;
            })
            .detach();
        }

        Ok(())
    }
}

#[async_trait(?Send)]
impl Domain for ClientDomain {
    fn domain_id(&self) -> DomainId {
        self.local_domain_id
    }

    fn domain_name(&self) -> &str {
        self.config.name()
    }

    async fn domain_label(&self) -> String {
        self.label.to_string()
    }

    async fn spawn_pane(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        self.ensure_mux_owner(mux)?;
        let workspace = mux.active_workspace();
        let rpc = inner.client.rpc_scope();
        let result = rpc
            .spawn_v2(SpawnV2 {
                domain: SpawnTabDomain::DefaultDomain,
                window_id: None,
                size,
                command,
                command_dir,
                workspace,
            })
            .await?;

        if !Self::sync_remote_topology(
            Arc::clone(mux),
            self,
            Arc::clone(&inner),
            &rpc,
            None,
        )
        .await?
        {
            bail!("client attachment retired while resolving spawned pane");
        }
        let (_tab, pane, _window_id) = Self::resolve_remote_spawn_entities(mux, &inner, result)?;
        Ok(pane)
    }

    /// Forward the request to the remote; we need to translate the local ids
    /// to those that match the remote for the request, resync the changed
    /// structure, and then translate the results back to local
    async fn move_pane_to_new_tab(
        &self,
        mux: &Arc<Mux>,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<Option<(Arc<Tab>, WindowId)>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        self.ensure_mux_owner(mux)?;
        let local_pane = mux
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let pane = local_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("pane_id {} is not a ClientPane", pane_id))?;
        if !pane.belongs_to_client(&inner) {
            bail!(
                "pane_id {} belongs to a different client attachment",
                pane_id
            );
        }

        let remote_window_id =
            window_id.and_then(|local_window| inner.local_to_remote_window(local_window));

        let rpc = inner.client.rpc_scope();
        let result = rpc
            .move_pane_to_new_tab(codec::MovePaneToNewTab {
                pane_id: pane.remote_pane_id,
                window_id: remote_window_id,
                workspace_for_new_window,
            })
            .await?;

        if !Self::sync_remote_topology(
            Arc::clone(mux),
            self,
            Arc::clone(&inner),
            &rpc,
            None,
        )
        .await?
        {
            bail!("client attachment retired while moving pane");
        }

        let local_tab_id = inner
            .remote_to_local_tab_id(result.tab_id)
            .ok_or_else(|| anyhow!("remote tab {} didn't resolve after resync", result.tab_id))?;

        let local_win_id = inner
            .remote_to_local_window(result.window_id)
            .ok_or_else(|| {
                anyhow!(
                    "remote window {} didn't resolve after resync",
                    result.window_id
                )
            })?;

        let tab = mux
            .get_tab(local_tab_id)
            .ok_or_else(|| anyhow!("local tab {local_tab_id} is invalid"))?;

        Ok(Some((tab, local_win_id)))
    }

    async fn spawn(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        self.ensure_mux_owner(mux)?;
        let workspace = workspace_for_spawn_window(mux, window);

        let rpc = inner.client.rpc_scope();
        let result = rpc
            .spawn_v2(SpawnV2 {
                domain: SpawnTabDomain::DefaultDomain,
                window_id: inner.local_to_remote_window(window),
                size,
                command,
                command_dir,
                workspace,
            })
            .await?;
        if !Self::sync_remote_topology(
            Arc::clone(mux),
            self,
            Arc::clone(&inner),
            &rpc,
            Some(window),
        )
        .await?
        {
            bail!("client attachment retired while resolving spawned tab");
        }
        let (tab, _pane, _window_id) = Self::resolve_remote_spawn_entities(mux, &inner, result)?;
        Ok(tab)
    }

    async fn split_pane(
        &self,
        mux: &Arc<Mux>,
        source: SplitSource,
        _tab_id: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        self.ensure_mux_owner(mux)?;
        let local_pane = mux
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let pane = local_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("pane_id {} is not a ClientPane", pane_id))?;
        if !pane.belongs_to_client(&inner) {
            bail!(
                "pane_id {} belongs to a different client attachment",
                pane_id
            );
        }

        let (command, command_dir, move_pane_id) = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => (command, command_dir, None),
            SplitSource::MovePane(move_pane_id) => {
                let move_pane = mux
                    .get_pane(move_pane_id)
                    .ok_or_else(|| anyhow!("move pane_id {} is invalid", move_pane_id))?;
                let move_pane = move_pane
                    .downcast_ref::<ClientPane>()
                    .ok_or_else(|| anyhow!("move pane_id {} is not a ClientPane", move_pane_id))?;
                if !move_pane.belongs_to_client(&inner) {
                    bail!(
                        "move pane_id {} belongs to a different client attachment",
                        move_pane_id
                    );
                }
                (None, None, Some(move_pane.remote_pane_id()))
            }
        };

        let rpc = inner.client.rpc_scope();
        let result = rpc
            .split_pane(SplitPane {
                domain: SpawnTabDomain::CurrentPaneDomain,
                pane_id: pane.remote_pane_id,
                split_request,
                command,
                command_dir,
                move_pane_id,
            })
            .await?;
        if !Self::sync_remote_topology(
            Arc::clone(mux),
            self,
            Arc::clone(&inner),
            &rpc,
            None,
        )
        .await?
        {
            bail!("client attachment retired while resolving split pane");
        }
        let (_tab, pane, _window_id) = Self::resolve_remote_spawn_entities(mux, &inner, result)?;
        Ok(pane)
    }

    async fn attach(
        &self,
        mux: &Arc<Mux>,
        owner_client_id: Option<Arc<ClientId>>,
        window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        self.ensure_mux_owner(mux)?;
        if self.state() == DomainState::Attached {
            if let Some(inner) = self.inner() {
                let rpc = inner.client.rpc_scope();
                let _ =
                    Self::sync_remote_topology(Arc::clone(mux), self, inner, &rpc, window_id)
                        .await?;
            }
            return Ok(());
        }

        let domain_id = self.local_domain_id;
        let config = self.config.clone();

        let activity = mux::activity::Activity::new_for_mux(mux);
        let ui = ConnectionUI::with_params(ConnectionUIParams {
            window_id,
            ..Default::default()
        });
        ui.title("FrankenTerm: Connecting...");

        ui.async_run_and_log_error({
            let ui = ui.clone();
            let mux = Arc::clone(mux);
            async move {
                let mut cloned_ui = ui.clone();
                let mux_owner = Arc::downgrade(&mux);
                let client = spawn_into_new_thread(move || match &config {
                    ClientDomainConfig::Unix(unix) => {
                        let initial = true;
                        let no_auto_start = false;
                        Client::new_unix_domain(
                            Some(domain_id),
                            unix,
                            initial,
                            &mut cloned_ui,
                            no_auto_start,
                            mux_owner,
                        )
                    }
                    ClientDomainConfig::Tls(tls) => {
                        Client::new_tls(domain_id, tls, &mut cloned_ui, mux_owner)
                    }
                    ClientDomainConfig::Ssh(ssh) => {
                        Client::new_ssh(domain_id, ssh, &mut cloned_ui, mux_owner)
                    }
                })
                .await?;

                ui.output_str("Checking server version\n");
                let rpc = client.rpc_scope();
                client.verify_version_compat_with_scope(&ui, &rpc).await?;

                ui.output_str("Version check OK!  Requesting pane list...\n");
                let panes = rpc.list_panes().await?;
                ui.output_str(&format!(
                    "Server has {} tabs.  Attaching to local UI...\n",
                    panes.tabs.len()
                ));
                ClientDomain::finish_attach(
                    &mux,
                    domain_id,
                    client,
                    panes,
                    owner_client_id,
                    window_id,
                )
            }
        })
        .await
        .map_err(|e| {
            ui.output_str(&format!("Error during attach: {:#}\n", e));
            e
        })?;

        ui.output_str("Attached!\n");
        drop(activity);
        ui.close();
        Ok(())
    }

    fn detachable(&self) -> bool {
        true
    }

    fn detach(&self) -> anyhow::Result<()> {
        self.perform_detach();
        Ok(())
    }

    fn state(&self) -> DomainState {
        if lock_or_recover(&self.inner, "client_domain_inner").is_some() {
            DomainState::Attached
        } else {
            DomainState::Detached
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MuxTestScope;
    use mux::tab::{PaneEntry, PaneNode};

    fn test_client_id(name: &str, pid: u32) -> Arc<ClientId> {
        Arc::new(ClientId {
            hostname: format!("{name}.local"),
            username: "testuser".to_string(),
            pid,
            epoch: 1000,
            id: 0,
            ssh_auth_sock: None,
        })
    }

    #[test]
    fn active_workspace_sync_request_only_targets_attached_owner_client() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);

        let owner = test_client_id("owner", 41_001);
        let other = test_client_id("other", 41_002);
        mux.register_client(Arc::clone(&owner));
        mux.register_client(Arc::clone(&other));
        mux.set_active_workspace_for_client(&owner, "owner-workspace");
        mux.set_active_workspace_for_client(&other, "other-workspace");

        assert_eq!(
            active_workspace_sync_request(Some(&owner), &owner, &mux),
            Some(codec::SetActiveWorkspace {
                workspace: "owner-workspace".to_string(),
            })
        );
        assert_eq!(
            active_workspace_sync_request(Some(&owner), &other, &mux),
            None
        );
        let same_value_stale_owner = Arc::new((*owner).clone());
        assert_eq!(
            active_workspace_sync_request(Some(&same_value_stale_owner), &owner, &mux),
            None,
            "an equal-valued stale client Arc must not inherit replacement authority"
        );
        assert_eq!(active_workspace_sync_request(None, &owner, &mux), None);
    }

    #[test]
    fn active_workspace_sync_request_tracks_renamed_owner_workspace() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);

        let owner = test_client_id("owner", 41_003);
        mux.register_client(Arc::clone(&owner));
        mux.set_active_workspace_for_client(&owner, "old-workspace");
        mux.rename_workspace("old-workspace", "renamed-workspace");

        assert_eq!(
            active_workspace_sync_request(Some(&owner), &owner, &mux),
            Some(codec::SetActiveWorkspace {
                workspace: "renamed-workspace".to_string(),
            })
        );
    }

    #[test]
    fn spawn_workspace_prefers_target_window_over_active_workspace() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);

        let owner = test_client_id("owner", 41_004);
        mux.register_client(Arc::clone(&owner));
        mux.replace_identity(Some(owner));
        mux.set_active_workspace("active-workspace");

        let target_window_id = *mux.new_empty_window(Some("target-workspace".to_string()), None);

        assert_eq!(
            workspace_for_spawn_window(&mux, target_window_id),
            "target-workspace"
        );
        assert_eq!(
            workspace_for_spawn_window(&mux, usize::MAX),
            "active-workspace"
        );
    }

    fn test_client_inner(local_domain_id: DomainId) -> Arc<ClientInner> {
        let unix = UnixDomain {
            name: "test-client-domain".to_string(),
            ..UnixDomain::default()
        };
        Arc::new(ClientInner::new(
            local_domain_id,
            Client::new_test_client(Some(local_domain_id), ClientDomainConfig::Unix(unix)),
            None,
            None,
            false,
        ))
    }

    #[test]
    fn stale_attachment_cannot_detach_a_same_domain_replacement() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let config = ClientDomainConfig::Unix(UnixDomain {
            name: "exact-detach-test".to_string(),
            ..UnixDomain::default()
        });
        let domain = Arc::new(
            ClientDomain::new(config, &mux).expect("client domain should allocate its subscriber"),
        );
        let registered: Arc<dyn Domain> = domain.clone();
        mux.add_domain(&registered)
            .expect("client domain should register");

        let stale = test_client_inner(domain.local_domain_id);
        let replacement = test_client_inner(domain.local_domain_id);
        *lock_or_recover(&domain.inner, "client_domain_inner") = Some(Arc::clone(&replacement));

        assert!(
            !domain.perform_detach_if_current(&stale),
            "an old reader must not detach a replacement ClientInner",
        );
        assert!(domain.inner_is_current(&replacement));
        assert!(!replacement.is_detached());
        assert!(
            mux.get_domain(domain.local_domain_id)
                .is_some_and(|current| Arc::ptr_eq(&current, &registered)),
            "rejected stale teardown must preserve the exact domain registration",
        );

        assert!(
            domain.perform_detach_if_current(&replacement),
            "the exact current attachment must remain detachable",
        );
        assert!(domain.inner().is_none());
        assert!(replacement.is_detached());
        assert!(mux.get_domain(domain.local_domain_id).is_none());
    }

    fn sample_remote_tab_listing() -> ListPanesResponse {
        ListPanesResponse {
            tabs: vec![PaneNode::Leaf(PaneEntry {
                window_id: 41,
                tab_id: 51,
                pane_id: 61,
                title: "remote shell".to_string(),
                size: TerminalSize {
                    cols: 120,
                    rows: 40,
                    pixel_width: 1200,
                    pixel_height: 800,
                    dpi: 96,
                },
                working_dir: None,
                alt_screen_active: true,
                is_active_pane: true,
                is_zoomed_pane: false,
                workspace: "ops".to_string(),
                cursor_pos: mux::renderable::StableCursorPosition::default(),
                physical_top: 0,
                top_row: 0,
                left_col: 0,
                tty_name: None,
            })],
            tab_titles: vec!["remote tab".to_string()],
            window_titles: HashMap::from([(41, "ops window".to_string())]),
        }
    }

    #[test]
    fn malformed_remote_tab_title_cardinality_is_rejected_before_topology_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_001);
        let mut listing = sample_remote_tab_listing();
        listing.tab_titles.clear();

        let err = ClientDomain::process_pane_list(&mux, Arc::clone(&inner), listing, None)
            .expect_err("mismatched tab/title cardinality must fail closed");

        assert!(
            err.to_string().contains("malformed ListPanes response"),
            "unexpected error: {:#}",
            err
        );
        assert!(mux.iter_panes().is_empty());
        assert!(mux.iter_windows().is_empty());
        assert!(
            lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window").is_empty()
        );
        assert!(lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab").is_empty());
        assert!(lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane").is_empty());
    }

    #[test]
    fn duplicate_remote_pane_identity_is_rejected_before_topology_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_003);
        let mut listing = sample_remote_tab_listing();
        listing.tabs.push(listing.tabs[0].clone());
        listing.tab_titles.push("duplicate remote tab".to_string());

        let err = ClientDomain::process_pane_list(&mux, Arc::clone(&inner), listing, None)
            .expect_err("duplicate remote pane identity must fail closed");

        assert!(
            err.to_string()
                .contains("remote pane 61 appears more than once"),
            "unexpected error: {:#}",
            err
        );
        assert!(mux.iter_panes().is_empty());
        assert!(mux.iter_windows().is_empty());
        assert!(
            lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window").is_empty()
        );
        assert!(lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab").is_empty());
        assert!(lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane").is_empty());
    }

    #[test]
    fn process_pane_list_uses_its_explicit_mux_when_the_global_mux_differs() {
        let scope = MuxTestScope::enter();
        let ambient_mux = Arc::new(Mux::new(None));
        scope.set_mux(&ambient_mux);
        let target_mux = Arc::new(Mux::new(None));
        let inner = test_client_inner(91_004);

        ClientDomain::process_pane_list(
            &target_mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            None,
        )
        .expect("explicit target mux should receive the remote topology");

        assert_eq!(target_mux.iter_panes().len(), 1);
        assert_eq!(target_mux.iter_windows().len(), 1);
        assert!(ambient_mux.iter_panes().is_empty());
        assert!(ambient_mux.iter_windows().is_empty());

        let local_tab_id = inner
            .remote_to_local_tab_id(51)
            .expect("remote tab should map into the explicit mux");
        assert_eq!(
            target_mux
                .get_tab(local_tab_id)
                .expect("mapped tab should exist in the explicit mux")
                .get_title(),
            "remote tab"
        );
    }

    #[test]
    fn duplicate_live_client_pane_mirrors_are_rejected_without_overwriting_index() {
        let mut by_remote_pane = HashMap::new();
        index_live_client_pane(&mut by_remote_pane, 61, 101)
            .expect("first live mirror should establish the identity");
        index_live_client_pane(&mut by_remote_pane, 61, 101)
            .expect("revisiting the exact same local pane is idempotent");

        let err = index_live_client_pane(&mut by_remote_pane, 61, 102)
            .expect_err("a second local mirror must fail closed");
        assert!(
            err.to_string()
                .contains("remote pane 61 is mirrored by local panes 101 and 102"),
            "unexpected error: {:#}",
            err
        );
        assert_eq!(by_remote_pane.get(&61), Some(&101));
    }

    #[test]
    fn stable_topology_resync_reuses_unconsumed_fallback_pane_id() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_002);

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            None,
        )
        .expect("initial remote topology should attach");
        assert!(
            lock_or_recover(&inner.spare_local_pane_ids, "spare_local_pane_ids").is_empty(),
            "the initial sync should consume its one reservation"
        );

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            None,
        )
        .expect("stable remote topology should resync");
        let spare_after_second_sync =
            lock_or_recover(&inner.spare_local_pane_ids, "spare_local_pane_ids").clone();
        assert_eq!(spare_after_second_sync.len(), 1);

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            None,
        )
        .expect("another stable remote topology should resync");
        assert_eq!(
            *lock_or_recover(&inner.spare_local_pane_ids, "spare_local_pane_ids"),
            spare_after_second_sync,
            "steady-state resync must return and reuse the same unconsumed fallback"
        );
        assert_eq!(mux.iter_panes().len(), 1);
    }

    /// Spawn a watchdog that aborts the test process if the body does not
    /// finish within `secs`. Used to turn a *deadlock* regression into a fast,
    /// obvious failure instead of a hung test binary (CI would otherwise just
    /// time out the whole suite with no signal).
    /// Cancellable watchdog. Returns a guard; when the guard drops (test
    /// finished, including on panic/unwind) the watchdog thread observes the
    /// flag and exits cleanly. This is critical: a fire-and-forget watchdog that
    /// outlives its test would `process::exit` during a *later* test if the whole
    /// suite runs slower than the timeout (e.g. on a busy CI/swarm host), killing
    /// the run spuriously. The watchdog only aborts if the guard is still alive
    /// at the deadline (i.e. the test really hung).
    #[must_use = "hold the guard for the duration of the test"]
    fn deadlock_watchdog(secs: u64, label: &'static str) -> WatchdogGuard {
        use std::sync::atomic::Ordering;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&done);
        let thread = std::thread::spawn(move || {
            for _ in 0..secs.saturating_mul(20) {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if flag.load(Ordering::SeqCst) {
                    return;
                }
            }
            if !flag.swap(true, Ordering::SeqCst) {
                eprintln!(
                    "WATCHDOG: `{label}` did not complete within {secs}s — likely a \
                     mux-lock deadlock regression (read guard held across a write lock)."
                );
                std::process::exit(97);
            }
        });
        WatchdogGuard {
            done,
            thread: Some(thread),
        }
    }

    struct WatchdogGuard {
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for WatchdogGuard {
        fn drop(&mut self) {
            self.done.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Regression guard for the remote-attach deadlock: `process_pane_list`
    /// takes the "reuse existing primary window with matching workspace" branch
    /// here (local window workspace "ops" == the listing's "ops"), which used to
    /// hold `Mux::get_window`'s read guard across `add_tab_to_window`'s write
    /// lock and self-deadlock parking_lot's RwLock. The watchdog makes a
    /// regression fail fast instead of hanging.
    #[test]
    fn process_pane_list_seeds_spawned_client_pane_alt_screen_state() {
        let scope = MuxTestScope::enter();
        let _wd = deadlock_watchdog(
            30,
            "process_pane_list_seeds_spawned_client_pane_alt_screen_state",
        );
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);

        let local_domain_id = alloc_domain_id();
        let inner = test_client_inner(local_domain_id);
        let local_window_id = *mux.new_empty_window(Some("ops".to_string()), None);

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            Some(local_window_id),
        )
        .expect("process_pane_list should seed remote pane state");

        let local_pane_id = inner
            .remote_to_local_pane_id(&mux, 61)
            .expect("remote pane should map locally");
        let pane = mux
            .get_pane(local_pane_id)
            .expect("local pane should exist after sync");
        let client_pane = pane
            .downcast_ref::<ClientPane>()
            .expect("pane should be a ClientPane");

        assert!(client_pane.is_alt_screen_active());
        assert_eq!(inner.remote_to_local_window(41), Some(local_window_id));
        assert!(mux.window_has_panes_in_domain(local_window_id, local_domain_id));

        let other_window_id = *mux.new_empty_window(Some("ops".to_string()), None);
        assert!(!mux.window_has_panes_in_domain(other_window_id, local_domain_id));
    }

    #[test]
    fn process_pane_list_keeps_workspace_mismatch_out_of_primary_window() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);

        let local_domain_id = alloc_domain_id();
        let inner = test_client_inner(local_domain_id);
        let requested_window_id = *mux.new_empty_window(Some("local-workspace".to_string()), None);

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            Some(requested_window_id),
        )
        .expect("process_pane_list should attach remote topology");

        let mapped_window_id = inner
            .remote_to_local_window(41)
            .expect("remote window should map locally");

        assert_ne!(mapped_window_id, requested_window_id);
        assert!(!mux.window_has_panes_in_domain(requested_window_id, local_domain_id));
        assert!(mux.window_has_panes_in_domain(mapped_window_id, local_domain_id));
    }

    #[test]
    fn resolve_remote_spawn_entities_returns_local_ids_after_sync() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);

        let local_domain_id = alloc_domain_id();
        let inner = test_client_inner(local_domain_id);
        let local_window_id = *mux.new_empty_window(Some("ops".to_string()), None);

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            Some(local_window_id),
        )
        .expect("process_pane_list should seed remote pane state");

        let (tab, pane, resolved_window_id) = ClientDomain::resolve_remote_spawn_entities(
            &mux,
            &inner,
            codec::SpawnResponse {
                pane_id: 61,
                tab_id: 51,
                window_id: 41,
                size: TerminalSize {
                    cols: 120,
                    rows: 40,
                    pixel_width: 1200,
                    pixel_height: 800,
                    dpi: 96,
                },
            },
        )
        .expect("spawn response should resolve through the synced remote topology");

        assert_eq!(resolved_window_id, local_window_id);
        assert_eq!(inner.remote_to_local_tab_id(51), Some(tab.tab_id()));
        assert_eq!(
            inner.remote_to_local_pane_id(&mux, 61),
            Some(pane.pane_id())
        );
        assert!(pane
            .downcast_ref::<ClientPane>()
            .expect("resolved pane should be a client pane")
            .is_alt_screen_active());
    }

    #[test]
    fn resolve_remote_spawn_entities_errors_when_remote_ids_do_not_resolve() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(alloc_domain_id());

        let error = match ClientDomain::resolve_remote_spawn_entities(
            &mux,
            &inner,
            codec::SpawnResponse {
                pane_id: 61,
                tab_id: 51,
                window_id: 41,
                size: TerminalSize {
                    cols: 120,
                    rows: 40,
                    pixel_width: 1200,
                    pixel_height: 800,
                    dpi: 96,
                },
            },
        ) {
            Ok(_) => {
                panic!("missing remote mappings should surface an explicit spawn resolution error")
            }
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("remote tab 51 didn't resolve after resync"),
            "unexpected error: {error:#}",
            error = error
        );
    }
}
