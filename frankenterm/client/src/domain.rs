use crate::client::{
    with_mux_rpc_bootstrap_timeout, Client, RpcConsumerKind, RpcGenerationAbortGuard,
    RpcGenerationScope,
};
use crate::pane::ClientPane;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use codec::{ListPanesResponse, SpawnV2, SplitPane};
use config::keyassignment::SpawnTabDomain;
use config::{SshDomain, TlsDomainClient, UnixDomain};
use mux::client::ClientId;
use mux::connui::{ConnectionUI, ConnectionUIParams};
use mux::domain::{alloc_domain_id, Domain, DomainId, DomainState};
use mux::pane::{reserve_pane_ids, Pane, PaneId};
use mux::tab::{
    prepare_pane_tree_from_arena_with_scratch, DomainFloatingPaneState, PaneArena, PaneArenaNode,
    PaneArenaPreparationScratch, PaneEntry, PaneNode, PreparedPaneTree, SplitRequest, Tab, TabId,
};
use mux::window::WindowId;
use mux::{
    CurrentPane, MoveCommitReceipt, Mux, MuxNotification, MuxWindowBuilder, PaneOperationGuard,
    PaneRegistrationHandle, SplitCommitReceipt,
};
use portable_pty::CommandBuilder;
use promise::spawn::spawn_into_new_thread;
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::hash::Hash;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use wezterm_term::TerminalSize;

/// One attachment-generation-scoped bijection between remote and local ids.
///
/// Keeping both directions behind the same mutex makes reverse lookups bounded
/// without creating a second lock or a torn forward/reverse update window.
/// Numeric local ids may be recycled by the mux, but the complete mapping is
/// owned by one [`ClientInner`] generation and is discarded with it.
struct ExactIdMappings<Remote, Local> {
    remote_to_local: HashMap<Remote, Local>,
    local_to_remote: HashMap<Local, Remote>,
    #[cfg(test)]
    reverse_lookup_probes: AtomicUsize,
}

impl<Remote, Local> Default for ExactIdMappings<Remote, Local> {
    fn default() -> Self {
        Self {
            remote_to_local: HashMap::new(),
            local_to_remote: HashMap::new(),
            #[cfg(test)]
            reverse_lookup_probes: AtomicUsize::new(0),
        }
    }
}

impl<Remote, Local> ExactIdMappings<Remote, Local>
where
    Remote: Copy + Eq + Hash,
    Local: Copy + Eq + Hash,
{
    fn len(&self) -> usize {
        self.remote_to_local.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.remote_to_local.is_empty()
    }

    fn get(&self, remote: &Remote) -> Option<&Local> {
        self.remote_to_local.get(remote)
    }

    fn get_remote(&self, local: &Local) -> Option<&Remote> {
        #[cfg(test)]
        {
            self.reverse_lookup_probes.fetch_add(1, Ordering::Relaxed);
        }
        self.local_to_remote.get(local)
    }

    fn iter(&self) -> impl Iterator<Item = (&Remote, &Local)> {
        self.remote_to_local.iter()
    }

    fn keys(&self) -> impl Iterator<Item = &Remote> {
        self.remote_to_local.keys()
    }

    /// Insert one authoritative mapping while retaining a strict bijection.
    ///
    /// Reusing either side retires its previous opposite-side association in
    /// the same critical section. This matches resync semantics: the newest
    /// exact attachment generation owns the local identity.
    fn insert(&mut self, remote: Remote, local: Local) -> Option<Local> {
        let prior_local = self.remote_to_local.get(&remote).copied();
        if prior_local == Some(local) {
            if self.local_to_remote.get(&local) != Some(&remote) {
                // This is corruption repair, so reserve before changing the
                // conflicting reverse edge. The forward cardinality cannot
                // grow on an idempotent insert.
                self.local_to_remote.reserve(1);
                if let Some(old_remote) = self.local_to_remote.remove(&local) {
                    self.remote_to_local.remove(&old_remote);
                }
                self.local_to_remote.insert(local, remote);
            }
            return prior_local;
        }

        // Under the maintained bijection, cardinality grows only when neither
        // side is currently owned. Reserve both tables before the first
        // semantic write so allocation cannot tear a recovered poisoned map.
        if prior_local.is_none() && !self.local_to_remote.contains_key(&local) {
            self.remote_to_local.reserve(1);
            self.local_to_remote.reserve(1);
        }
        if let Some(old_local) = prior_local {
            self.local_to_remote.remove(&old_local);
        }
        if let Some(old_remote) = self.local_to_remote.remove(&local) {
            self.remote_to_local.remove(&old_remote);
        }
        self.remote_to_local.insert(remote, local);
        self.local_to_remote.insert(local, remote);
        prior_local
    }

    fn remove(&mut self, remote: &Remote) -> Option<Local> {
        let local = self.remote_to_local.remove(remote)?;
        let removed_remote = self.local_to_remote.remove(&local);
        debug_assert!(removed_remote == Some(*remote));
        Some(local)
    }

    fn retain(&mut self, mut retain: impl FnMut(&Remote, &Local) -> bool) {
        // Evaluate caller policy completely before the first semantic write.
        // HashMap::retain mutates incrementally, so a panicking predicate could
        // otherwise poison the mutex after changing only the forward half.
        let mut removals = Vec::with_capacity(self.remote_to_local.len());
        for (remote, local) in &self.remote_to_local {
            if !retain(remote, local) {
                removals.push(*remote);
            }
        }
        for remote in removals {
            self.remove(&remote);
        }
    }

    fn extend(&mut self, mappings: impl IntoIterator<Item = (Remote, Local)>) {
        for (remote, local) in mappings {
            self.insert(remote, local);
        }
    }

    #[cfg(test)]
    fn insert_forward_alias_for_test(&mut self, remote: Remote, local: Local) {
        self.remote_to_local.insert(remote, local);
    }

    #[cfg(test)]
    fn reverse_lookup_probes(&self) -> usize {
        self.reverse_lookup_probes.load(Ordering::Relaxed)
    }
}

pub struct ClientInner {
    pub client: Client,
    pub local_domain_id: DomainId,
    owner_client_id: Option<Arc<ClientId>>,
    pub local_echo_threshold_ms: Option<u64>,
    pub overlay_lag_indicator: bool,
    remote_to_local_window: Mutex<ExactIdMappings<WindowId, WindowId>>,
    remote_to_local_tab: Mutex<ExactIdMappings<TabId, TabId>>,
    remote_to_local_pane: Mutex<HashMap<PaneId, PaneId>>,
    spare_local_pane_ids: Mutex<Vec<PaneId>>,
    pub focused_remote_pane_id: Mutex<Option<PaneId>>,
    detached: AtomicBool,
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
    pane_tab_ids: &mut HashMap<PaneId, TabId>,
) -> anyhow::Result<()> {
    match node {
        PaneNode::Empty => {}
        PaneNode::Split { left, right, .. } => {
            collect_remote_pane_ids(
                left,
                expected_tree_identity,
                seen_pane_ids,
                pane_ids,
                pane_tab_ids,
            )?;
            collect_remote_pane_ids(
                right,
                expected_tree_identity,
                seen_pane_ids,
                pane_ids,
                pane_tab_ids,
            )?;
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
            if pane_tab_ids.insert(entry.pane_id, entry.tab_id).is_some() {
                bail!(
                    "malformed ListPanes response: remote pane {} has conflicting tab owners",
                    entry.pane_id
                );
            }
            pane_ids.push(entry.pane_id);
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PaneArenaTabPlan {
    node_count: usize,
    root_size: TerminalSize,
    remote_window_id: WindowId,
    remote_tab_id: TabId,
}

struct PreparedPaneArenaTab {
    plan: PaneArenaTabPlan,
    workspace: String,
    tab_title: String,
    tree: PreparedPaneTree,
}

struct StagedPaneArenaTab {
    prepared: PreparedPaneArenaTab,
    tab: Arc<Tab>,
}

#[derive(Default)]
struct PendingPaneArenaPublication {
    new_panes: Vec<(PaneId, Arc<dyn Pane>)>,
    existing_sync: Vec<(Arc<dyn Pane>, bool)>,
}

struct PaneArenaPublicationRollback {
    mux: Arc<Mux>,
    pane_registrations: Vec<PaneRegistrationHandle>,
    new_tabs: Vec<Arc<Tab>>,
    new_windows: Vec<MuxWindowBuilder>,
    committed: bool,
}

impl PaneArenaPublicationRollback {
    fn new(mux: &Arc<Mux>) -> Self {
        Self {
            mux: Arc::clone(mux),
            pane_registrations: Vec::new(),
            new_tabs: Vec::new(),
            new_windows: Vec::new(),
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PaneArenaPublicationRollback {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for tab in self.new_tabs.drain(..).rev() {
            self.mux.remove_tab_local_only_if_same(&tab);
        }
        for window in self.new_windows.drain(..).rev() {
            window.cancel();
        }
        for registration in self.pane_registrations.drain(..).rev() {
            registration.detach_local_if_current();
        }
    }
}

struct PaneArenaPreflight {
    tabs: Vec<PaneArenaTabPlan>,
    remote_pane_ids: Vec<PaneId>,
    remote_pane_tabs: Vec<(PaneId, TabId)>,
    window_ids: Vec<WindowId>,
}

/// Validate every descriptor, child edge, and remote identity before a direct
/// flat-arena application is allowed to reserve an identifier or mutate the
/// mux. `PaneArena::from_unvalidated_parts` is public for codec admission, so
/// the dormant client application seam must not assume that its caller used
/// the codec validator.
fn preflight_pane_arena(panes: &PaneArena) -> anyhow::Result<PaneArenaPreflight> {
    codec::validate_ordered_pane_arena(panes)
        .context("validate ordered pane arena resource and topology admission")?;
    let mut cursor = 0usize;
    let mut tabs = Vec::new();
    tabs.try_reserve_exact(panes.trees().len())
        .context("reserve ordered pane arena tab preflight")?;
    let mut remote_pane_ids = Vec::new();
    remote_pane_ids
        .try_reserve_exact(panes.nodes().len().div_ceil(2))
        .context("reserve ordered pane arena pane identities")?;
    let mut remote_pane_tabs = Vec::new();
    remote_pane_tabs
        .try_reserve_exact(panes.nodes().len().div_ceil(2))
        .context("reserve ordered pane arena pane/tab identities")?;
    let mut seen_remote_pane_ids = HashSet::new();
    seen_remote_pane_ids
        .try_reserve(panes.nodes().len().div_ceil(2))
        .context("reserve ordered pane arena unique pane identities")?;
    let mut seen_remote_tab_ids = HashSet::new();
    seen_remote_tab_ids
        .try_reserve(panes.trees().len())
        .context("reserve ordered pane arena unique tab identities")?;
    let mut seen_remote_window_ids = HashSet::new();
    seen_remote_window_ids
        .try_reserve(panes.window_titles().len())
        .context("reserve ordered pane arena unique window identities")?;

    for (tree_index, descriptor) in panes.trees().iter().enumerate() {
        let node_count = usize::try_from(descriptor.node_count).with_context(|| {
            format!("ordered pane arena tree {tree_index} node count does not fit usize")
        })?;
        let root_index = descriptor
            .root_index
            .ok_or_else(|| anyhow!("ordered pane arena tree {tree_index} has no root index"))?;
        let root_index = usize::try_from(root_index).with_context(|| {
            format!("ordered pane arena tree {tree_index} root index does not fit usize")
        })?;
        if node_count == 0 || root_index != cursor {
            bail!(
                "ordered pane arena tree {tree_index} has root {root_index} and {node_count} \
                 nodes; expected one non-empty range rooted at {cursor}"
            );
        }
        let arena_end = cursor
            .checked_add(node_count)
            .ok_or_else(|| anyhow!("ordered pane arena tree {tree_index} range overflows usize"))?;
        if arena_end > panes.nodes().len() {
            bail!(
                "ordered pane arena tree {tree_index} ends at {arena_end}, beyond {} nodes",
                panes.nodes().len()
            );
        }

        let root_size = match &panes.nodes()[root_index] {
            PaneArenaNode::Empty => {
                bail!("ordered pane arena tree {tree_index} has an empty root")
            }
            PaneArenaNode::Split { node, .. } => node.size(),
            PaneArenaNode::Leaf(entry) => entry.size,
        };
        let mut tree_identity = None;
        let mut workspace: Option<&str> = None;
        for node in &panes.nodes()[root_index..arena_end] {
            if let PaneArenaNode::Leaf(entry) = node {
                if matches!(u64::try_from(entry.window_id), Ok(u64::MAX))
                    || matches!(u64::try_from(entry.tab_id), Ok(u64::MAX))
                    || entry.pane_id == usize::MAX
                {
                    bail!(
                        "ordered pane arena tree {tree_index} uses a reserved terminal \
                         window, tab, or pane identity"
                    );
                }
                let identity = (entry.window_id, entry.tab_id);
                if tree_identity.is_some_and(|expected| expected != identity) {
                    bail!(
                        "ordered pane arena tree {tree_index} mixes window/tab identities \
                         {tree_identity:?} and {identity:?}"
                    );
                }
                tree_identity = Some(identity);
                if workspace
                    .as_ref()
                    .is_some_and(|expected| *expected != entry.workspace.as_str())
                {
                    bail!("ordered pane arena tree {tree_index} mixes workspace identities");
                }
                if workspace.is_none() {
                    workspace = Some(entry.workspace.as_str());
                }
                if !seen_remote_pane_ids.insert(entry.pane_id) {
                    bail!(
                        "ordered pane arena remote pane {} appears more than once",
                        entry.pane_id
                    );
                }
                remote_pane_ids.push(entry.pane_id);
                remote_pane_tabs.push((entry.pane_id, entry.tab_id));
            }
        }
        let (remote_window_id, remote_tab_id) = tree_identity
            .ok_or_else(|| anyhow!("ordered pane arena tree {tree_index} has no pane identity"))?;
        if !seen_remote_tab_ids.insert(remote_tab_id) {
            bail!("ordered pane arena remote tab {remote_tab_id} appears more than once");
        }
        workspace.ok_or_else(|| {
            anyhow!("ordered pane arena tree {tree_index} has no workspace authority")
        })?;
        seen_remote_window_ids.insert(remote_window_id);
        tabs.push(PaneArenaTabPlan {
            node_count,
            root_size,
            remote_window_id,
            remote_tab_id,
        });
        cursor = arena_end;
    }

    if cursor != panes.nodes().len() {
        bail!(
            "ordered pane arena descriptors reference {cursor} of {} nodes",
            panes.nodes().len()
        );
    }

    let mut window_ids = Vec::new();
    window_ids
        .try_reserve_exact(panes.window_titles().len())
        .context("reserve ordered pane arena window-title preflight")?;
    let mut title_window_ids = HashSet::new();
    title_window_ids
        .try_reserve(panes.window_titles().len())
        .context("reserve ordered pane arena title identities")?;
    let mut prior_window_id = None;
    for entry in panes.window_titles() {
        if entry.window_id == u64::MAX {
            bail!("ordered pane arena window title uses the reserved terminal identity");
        }
        let remote_window_id = usize::try_from(entry.window_id).with_context(|| {
            format!(
                "ordered pane arena window id {} does not fit this process",
                entry.window_id
            )
        })?;
        if prior_window_id.is_some_and(|prior| prior >= remote_window_id) {
            bail!("ordered pane arena window titles are not in canonical id order");
        }
        prior_window_id = Some(remote_window_id);
        if !title_window_ids.insert(remote_window_id) {
            bail!("ordered pane arena repeats window title {remote_window_id}");
        }
        window_ids.push(remote_window_id);
    }
    if !seen_remote_window_ids.is_subset(&title_window_ids) {
        bail!("ordered pane arena has a pane-tree window without matching title authority");
    }

    Ok(PaneArenaPreflight {
        tabs,
        remote_pane_ids,
        remote_pane_tabs,
        window_ids,
    })
}

/// The flat path can append new mirrors in descriptor order, but it cannot
/// soundly rewrite an already-published window permutation with the current
/// client-facing mux API. Reject such a snapshot before pane preparation has
/// any registration side effect.
fn ensure_pane_arena_append_order_is_sound(
    mux: &Mux,
    inner: &ClientInner,
    tabs: &[PaneArenaTabPlan],
    remote_pane_tabs: &[(PaneId, TabId)],
    window_ids: &[WindowId],
) -> anyhow::Result<()> {
    let mut desired_by_window: HashMap<WindowId, Vec<TabId>> = HashMap::new();
    desired_by_window
        .try_reserve(window_ids.len())
        .context("reserve ordered pane arena desired windows")?;
    let mut desired_tabs = HashSet::new();
    desired_tabs
        .try_reserve(tabs.len())
        .context("reserve ordered pane arena desired tabs")?;
    for tab in tabs {
        desired_tabs.insert(tab.remote_tab_id);
        desired_by_window
            .entry(tab.remote_window_id)
            .or_default()
            .push(tab.remote_tab_id);
    }

    let mut desired_windows = HashSet::new();
    desired_windows
        .try_reserve(window_ids.len())
        .context("reserve ordered pane arena desired window identities")?;
    desired_windows.extend(window_ids.iter().copied());

    let mut desired_pane_tabs = HashMap::new();
    desired_pane_tabs
        .try_reserve(remote_pane_tabs.len())
        .context("reserve ordered pane arena desired pane ownership")?;
    for &(remote_pane_id, remote_tab_id) in remote_pane_tabs {
        if desired_pane_tabs
            .insert(remote_pane_id, remote_tab_id)
            .is_some()
        {
            bail!("ordered pane arena repeats remote pane {remote_pane_id}");
        }
    }

    let tab_mappings = {
        let mappings = lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab");
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(mappings.len())
            .context("reserve ordered pane arena tab-mapping snapshot")?;
        snapshot.extend(mappings.iter().map(|(remote, local)| (*remote, *local)));
        snapshot
    };
    let window_mappings = {
        let mappings = lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window");
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(mappings.len())
            .context("reserve ordered pane arena window-mapping snapshot")?;
        snapshot.extend(mappings.iter().map(|(remote, local)| (*remote, *local)));
        snapshot
    };

    let mut local_to_remote_tab = HashMap::new();
    local_to_remote_tab
        .try_reserve(tab_mappings.len())
        .context("reserve ordered pane arena reverse tab mappings")?;
    let mut remote_to_local_tab = HashMap::new();
    remote_to_local_tab
        .try_reserve(tab_mappings.len())
        .context("reserve ordered pane arena tab mappings")?;
    for &(remote_tab_id, local_tab_id) in &tab_mappings {
        if let Some(prior_remote_tab_id) = local_to_remote_tab.insert(local_tab_id, remote_tab_id) {
            bail!(
                "ordered pane arena mappings alias remote tabs {prior_remote_tab_id} and \
                 {remote_tab_id} onto local tab {local_tab_id}"
            );
        }
        remote_to_local_tab.insert(remote_tab_id, local_tab_id);
    }

    let mut local_to_remote_window = HashMap::new();
    local_to_remote_window
        .try_reserve(window_mappings.len())
        .context("reserve ordered pane arena reverse window mappings")?;
    let mut remote_to_local_window = HashMap::new();
    remote_to_local_window
        .try_reserve(window_mappings.len())
        .context("reserve ordered pane arena window mappings")?;
    for &(remote_window_id, local_window_id) in &window_mappings {
        if let Some(prior_remote_window_id) =
            local_to_remote_window.insert(local_window_id, remote_window_id)
        {
            bail!(
                "ordered pane arena mappings alias remote windows {prior_remote_window_id} and \
                 {remote_window_id} onto local window {local_window_id}"
            );
        }
        remote_to_local_window.insert(remote_window_id, local_window_id);
    }

    let mut parent_by_local_tab = HashMap::new();
    parent_by_local_tab
        .try_reserve(tab_mappings.len().max(tabs.len()))
        .context("reserve ordered pane arena tab-parent index")?;
    for local_window_id in mux.iter_windows() {
        let window = mux.get_window(local_window_id).ok_or_else(|| {
            anyhow!("local window {local_window_id} disappeared while indexing tab parents")
        })?;
        for tab in window.iter() {
            if let Some(prior_window_id) = parent_by_local_tab.insert(tab.tab_id(), local_window_id)
            {
                bail!(
                    "local tab {} is attached to windows {prior_window_id} and {local_window_id}",
                    tab.tab_id()
                );
            }
        }
    }

    let mut live_remote_panes = HashMap::new();
    live_remote_panes
        .try_reserve(remote_pane_tabs.len())
        .context("reserve ordered pane arena live pane ownership")?;
    for pane in mux.iter_panes() {
        let Some(client_pane) = pane.downcast_ref::<ClientPane>() else {
            continue;
        };
        if !client_pane.belongs_to_client(inner) {
            continue;
        }
        let remote_pane_id = client_pane.remote_pane_id();
        if let Some(prior_local_pane_id) = live_remote_panes.insert(remote_pane_id, pane.pane_id())
        {
            bail!(
                "remote pane {remote_pane_id} is mirrored by local panes \
                 {prior_local_pane_id} and {}",
                pane.pane_id()
            );
        }
        let Some(&desired_remote_tab_id) = desired_pane_tabs.get(&remote_pane_id) else {
            bail!(
                "ordered pane arena removes live remote pane {remote_pane_id}; atomic stale-pane \
                 removal is required"
            );
        };
        if client_pane.remote_tab_id != desired_remote_tab_id {
            bail!(
                "ordered pane arena moves remote pane {remote_pane_id} from tab {} to tab \
                 {desired_remote_tab_id}; atomic pane migration is required",
                client_pane.remote_tab_id
            );
        }
    }

    for &(remote_tab_id, local_tab_id) in &tab_mappings {
        let Some(tab) = mux.get_tab(local_tab_id) else {
            continue;
        };
        if !desired_tabs.contains(&remote_tab_id) {
            bail!(
                "ordered pane arena removes live remote tab {remote_tab_id}; atomic stale-tab \
                 removal is required"
            );
        }
        let panes = tab.iter_all_panes();
        if panes.is_empty() {
            bail!(
                "ordered pane arena mapping {remote_tab_id}->{local_tab_id} targets an empty \
                 local tab whose client ownership cannot be proven"
            );
        }
        for pane in panes {
            let Some(client_pane) = pane.downcast_ref::<ClientPane>() else {
                bail!(
                    "ordered pane arena mapping {remote_tab_id}->{local_tab_id} targets a tab \
                     containing a non-client pane"
                );
            };
            if !client_pane.belongs_to_client(inner) || client_pane.remote_tab_id != remote_tab_id {
                bail!(
                    "ordered pane arena mapping {remote_tab_id}->{local_tab_id} does not belong \
                     exactly to this client and remote tab"
                );
            }
        }
    }

    for &(remote_window_id, local_window_id) in &window_mappings {
        let Some(window) = mux.get_window(local_window_id) else {
            continue;
        };
        if !desired_windows.contains(&remote_window_id) {
            bail!(
                "ordered pane arena removes live remote window {remote_window_id}; atomic \
                 stale-window removal is required"
            );
        }
        if window.is_empty() {
            bail!(
                "ordered pane arena mapping {remote_window_id}->{local_window_id} targets an \
                 empty local window whose client ownership cannot be proven"
            );
        }
    }

    for remote_window_id in window_ids {
        if desired_by_window.contains_key(remote_window_id) {
            continue;
        }
        bail!(
            "ordered pane arena window {remote_window_id} has no tab tree; applying a title-only \
             window requires exact ordered workspace and client ownership authority"
        );
    }

    for (remote_window_id, desired_remote_tabs) in desired_by_window {
        let live_local_window_id = remote_to_local_window
            .get(&remote_window_id)
            .copied()
            .filter(|id| mux.get_window(*id).is_some());
        let Some(local_window_id) = live_local_window_id else {
            for remote_tab_id in &desired_remote_tabs {
                let Some(local_tab_id) = remote_to_local_tab.get(remote_tab_id).copied() else {
                    continue;
                };
                if mux.get_tab(local_tab_id).is_some() {
                    bail!(
                        "ordered pane arena remote window {remote_window_id} has no live local \
                         window mapping, but tab {remote_tab_id} already has a live local mirror; \
                         atomic snapshot mirroring is required"
                    );
                }
            }
            continue;
        };

        let window = mux.get_window(local_window_id).ok_or_else(|| {
            anyhow!("local window {local_window_id} disappeared during preflight")
        })?;
        if window.len() > desired_remote_tabs.len() {
            bail!(
                "ordered pane arena window {remote_window_id} requires an atomic existing-window \
                 reorder because it has {} attached tabs but authority has {}",
                window.len(),
                desired_remote_tabs.len()
            );
        }
        for (index, tab) in window.iter().enumerate() {
            let attached_remote_tab =
                local_to_remote_tab
                    .get(&tab.tab_id())
                    .copied()
                    .ok_or_else(|| {
                        anyhow!(
                        "ordered pane arena mapped window {remote_window_id} contains unmapped or \
                         foreign local tab {}",
                        tab.tab_id()
                    )
                    })?;
            if desired_remote_tabs[index] != attached_remote_tab {
                bail!(
                    "ordered pane arena window {remote_window_id} requires an atomic existing-window \
                     reorder at index {index}: attached remote tab {attached_remote_tab}, desired {}",
                    desired_remote_tabs[index]
                );
            }
        }

        for remote_tab_id in &desired_remote_tabs {
            let Some(local_tab_id) = remote_to_local_tab.get(remote_tab_id).copied() else {
                continue;
            };
            if mux.get_tab(local_tab_id).is_none() {
                continue;
            }
            if let Some(parent) = parent_by_local_tab.get(&local_tab_id).copied() {
                if parent != local_window_id {
                    bail!(
                        "ordered pane arena tab {remote_tab_id} is attached to local window \
                         {parent}, not mapped window {local_window_id}; atomic snapshot mirroring \
                        is required"
                    );
                }
            } else {
                bail!(
                    "ordered pane arena tab {remote_tab_id} has a live but unattached local \
                     mirror; transactional attachment rollback is required"
                );
            }
        }
    }
    Ok(())
}

fn resolve_pane_arena_entry(
    mux: &Arc<Mux>,
    inner: &Arc<ClientInner>,
    entry: PaneEntry,
    remote_panes_to_forget: &mut HashSet<PaneId>,
    local_pane_ids_by_remote: &mut HashMap<PaneId, PaneId>,
    reserved_local_pane_ids: &mut LocalPaneIdReservations<'_>,
    pending: &mut PendingPaneArenaPublication,
) -> anyhow::Result<Arc<dyn Pane>> {
    remote_panes_to_forget.remove(&entry.pane_id);
    let pane = if let Some(local_pane_id) = local_pane_ids_by_remote.get(&entry.pane_id).copied() {
        match mux.get_pane(local_pane_id) {
            Some(pane)
                if pane
                    .downcast_ref::<ClientPane>()
                    .is_some_and(|client_pane| {
                        client_pane.belongs_to_client(inner)
                            && client_pane.remote_pane_id() == entry.pane_id
                            && client_pane.remote_tab_id == entry.tab_id
                    }) =>
            {
                pending
                    .existing_sync
                    .push((Arc::clone(&pane), entry.alt_screen_active));
                pane
            }
            Some(_) | None => {
                let local_pane_id =
                    reserved_local_pane_ids.take(entry.pane_id).ok_or_else(|| {
                        anyhow!(
                            "remote pane {} needs a local identifier, but no identifier was \
                             reserved",
                            entry.pane_id
                        )
                    })?;
                let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
                    inner,
                    local_pane_id,
                    entry.tab_id,
                    entry.pane_id,
                    entry.size,
                    &entry.title,
                    entry.alt_screen_active,
                ));
                local_pane_ids_by_remote.insert(entry.pane_id, local_pane_id);
                pending.new_panes.push((entry.pane_id, Arc::clone(&pane)));
                pane
            }
        }
    } else {
        let local_pane_id = reserved_local_pane_ids.take(entry.pane_id).ok_or_else(|| {
            anyhow!(
                "remote pane {} needs a local identifier, but no identifier was reserved",
                entry.pane_id
            )
        })?;
        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
            inner,
            local_pane_id,
            entry.tab_id,
            entry.pane_id,
            entry.size,
            &entry.title,
            entry.alt_screen_active,
        ));
        local_pane_ids_by_remote.insert(entry.pane_id, local_pane_id);
        pending.new_panes.push((entry.pane_id, Arc::clone(&pane)));
        pane
    };
    Ok(pane)
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

    fn restore(&mut self, remote_pane_id: PaneId, local_pane_id: PaneId) {
        match self.by_remote_pane.entry(remote_pane_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(local_pane_id);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                log::error!(
                    "remote pane {remote_pane_id} already retained a local identifier \
                     reservation; returning duplicate reservation {local_pane_id} to the spare pool"
                );
                lock_or_recover(self.spare_pool, "spare_local_pane_ids").push(local_pane_id);
            }
        }
    }
}

struct PendingFloatingPaneMappings<'reservations, 'pool> {
    reservations: &'reservations mut LocalPaneIdReservations<'pool>,
    mappings: Vec<(PaneId, PaneId)>,
    committed: bool,
}

impl<'reservations, 'pool> PendingFloatingPaneMappings<'reservations, 'pool> {
    fn new(
        reservations: &'reservations mut LocalPaneIdReservations<'pool>,
        capacity: usize,
    ) -> anyhow::Result<Self> {
        let mut mappings = Vec::new();
        mappings
            .try_reserve_exact(capacity)
            .context("reserve pending floating-pane mappings")?;
        Ok(Self {
            reservations,
            mappings,
            committed: false,
        })
    }

    fn take(&mut self, remote_pane_id: PaneId) -> Option<PaneId> {
        let local_pane_id = self.reservations.take(remote_pane_id)?;
        self.mappings.push((remote_pane_id, local_pane_id));
        Some(local_pane_id)
    }

    fn commit(mut self) -> Vec<(PaneId, PaneId)> {
        self.committed = true;
        std::mem::take(&mut self.mappings)
    }
}

impl Drop for PendingFloatingPaneMappings<'_, '_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for (remote_pane_id, local_pane_id) in self.mappings.drain(..) {
            self.reservations.restore(remote_pane_id, local_pane_id);
        }
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
        map.get_remote(&local_tab_id).copied()
    }

    fn local_to_remote_window(&self, local_window_id: WindowId) -> Option<WindowId> {
        let map = lock_or_recover(&self.remote_to_local_window, "remote_to_local_window");
        map.get_remote(&local_window_id).copied()
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
            remote_to_local_window: Mutex::new(ExactIdMappings::default()),
            remote_to_local_tab: Mutex::new(ExactIdMappings::default()),
            remote_to_local_pane: Mutex::new(HashMap::new()),
            spare_local_pane_ids: Mutex::new(Vec::new()),
            focused_remote_pane_id: Mutex::new(None),
            detached: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    fn mark_detached(&self) {
        self.detached.store(true, Ordering::Release);
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

struct InitialAttachmentRequest {
    owner_client_id: Option<Arc<ClientId>>,
    primary_window_id: Option<WindowId>,
}

impl Drop for InitialAttachmentClaim<'_> {
    fn drop(&mut self) {
        self.pending.store(false, Ordering::Release);
    }
}

struct InitialAttachmentCleanup {
    mux: Arc<Mux>,
    domain_registration: Arc<dyn Domain>,
    inner: Arc<ClientInner>,
    rpc: RpcGenerationScope,
    armed: bool,
}

impl InitialAttachmentCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }

    fn cleanup_if_current(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;

        let mux = Arc::clone(&self.mux);
        let domain_registration = Arc::clone(&self.domain_registration);
        let inner = Arc::clone(&self.inner);
        let rpc = self.rpc.clone();
        let _ = rpc.commit_sync(RpcConsumerKind::InitialAttachmentCleanup, || {
            let _ = inner.client.abort_rpc_transport_generation(
                &rpc,
                "initial attachment preparation failed or was cancelled",
            );
            inner.mark_detached();

            let Some(domain) = domain_registration.downcast_ref::<ClientDomain>() else {
                return;
            };
            if domain.perform_detach_if_current(&inner) {
                return;
            }
            let attachment_slot_is_empty =
                lock_or_recover(&domain.inner, "client_domain_inner").is_none();
            if attachment_slot_is_empty {
                domain.retired.store(true, Ordering::Release);
                let _ = mux.domain_was_detached_if_same(&domain_registration);
            }
        });
    }
}

impl Drop for InitialAttachmentCleanup {
    fn drop(&mut self) {
        self.cleanup_if_current();
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
        MuxNotification::WindowWorkspaceChanged {
            window_id,
            workspace,
        } => {
            // Defer the RPC so the notification callback never performs
            // domain lookup or transport work while the originating mux
            // mutation is still unwinding.
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
                    let Some(remote_window_id) = client_domain.local_to_remote_window_id(window_id)
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
        let mut abort_guard =
            rpc.abort_guard("successor mux RPC bootstrap failed, timed out, or was cancelled")?;
        let result = with_mux_rpc_bootstrap_timeout(Self::reattach_if_current_inner(
            mux,
            domain,
            expected,
            rpc,
            &abort_guard,
            ui,
        ))
        .await;
        if result.is_ok() {
            abort_guard.disarm();
        }
        result
    }

    async fn reattach_if_current_inner(
        mux: Arc<Mux>,
        domain: Arc<dyn Domain>,
        expected: Arc<ClientInner>,
        rpc: RpcGenerationScope,
        readiness_guard: &RpcGenerationAbortGuard,
        ui: ConnectionUI,
    ) -> anyhow::Result<()> {
        let domain_id = domain.domain_id();
        let current = mux
            .get_domain(domain_id)
            .is_some_and(|candidate| Arc::ptr_eq(&candidate, &domain));
        if !current {
            let _ = expected
                .client
                .abort_rpc_transport_generation(&rpc, "reconnect target domain retired");
            return Ok(());
        }
        let client_domain = domain
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", domain_id))?;
        if client_domain
            .initial_attachment_pending
            .load(Ordering::Acquire)
        {
            let _ = expected.client.abort_rpc_transport_generation(
                &rpc,
                "successor arrived before initial attachment transaction retired",
            );
            bail!(
                "client domain {domain_id} cannot publish a successor while its initial \
                 attachment transaction is still pending"
            );
        }
        if !client_domain.inner_is_current(&expected) {
            let _ = expected
                .client
                .abort_rpc_transport_generation(&rpc, "reconnect client attachment retired");
            return Ok(());
        }

        // Every physical server connection owns a fresh SessionHandler with no
        // client identity. Re-establish codec compatibility and SetClientId on
        // the exact successor generation before any topology or workspace RPC.
        if let Err(error) = expected
            .client
            .verify_version_compat_with_scope(&ui, &rpc)
            .await
        {
            let _ = expected
                .client
                .abort_rpc_transport_generation(&rpc, "successor mux RPC bootstrap failed");
            return Err(error);
        }

        let topology_current = match Self::sync_remote_topology(
            Arc::clone(&mux),
            client_domain,
            Arc::clone(&expected),
            &rpc,
            None,
        )
        .await
        {
            Ok(current) => current,
            Err(error) => {
                let _ = expected
                    .client
                    .abort_rpc_transport_generation(&rpc, "successor topology bootstrap failed");
                return Err(error);
            }
        };
        if !topology_current {
            let _ = expected.client.abort_rpc_transport_generation(
                &rpc,
                "successor topology bootstrap lost attachment authority",
            );
            return Ok(());
        }

        if !client_inner_is_current(&mux, &domain, &expected) {
            let _ = expected.client.abort_rpc_transport_generation(
                &rpc,
                "successor topology bootstrap lost domain authority",
            );
            return Ok(());
        }
        if let Some(request) = current_active_workspace_sync(&expected, &mux) {
            if let Err(error) = rpc.set_active_workspace(request).await {
                let _ = expected
                    .client
                    .abort_rpc_transport_generation(&rpc, "successor workspace bootstrap failed");
                return Err(error).context("synchronizing the successor active workspace");
            }
        }
        if !client_inner_is_current(&mux, &domain, &expected) {
            let _ = expected.client.abort_rpc_transport_generation(
                &rpc,
                "successor workspace bootstrap lost domain authority",
            );
            return Ok(());
        }

        expected
            .client
            .publish_rpc_transport_ready(&rpc, readiness_guard)
            .await?;
        ui.close();
        Ok(())
    }

    pub(crate) async fn resync_if_current(
        &self,
        mux: Arc<Mux>,
        expected: Arc<ClientInner>,
        rpc: &RpcGenerationScope,
    ) -> anyhow::Result<bool> {
        if self.inner_is_current(&expected) {
            return Self::sync_remote_topology(mux, self, expected, rpc, None).await;
        }
        Ok(false)
    }

    async fn sync_remote_topology(
        mux: Arc<Mux>,
        domain: &Self,
        inner: Arc<ClientInner>,
        rpc: &RpcGenerationScope,
        primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<bool> {
        let incarnation_is_current = || {
            !inner.is_detached()
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
        let topology_current = rpc
            .with_coherent_topology_snapshot(RpcConsumerKind::TopologySnapshot, |panes| {
                if !incarnation_is_current() {
                    bail!("client attachment retired before coherent topology application");
                }
                Self::process_pane_list(&mux, Arc::clone(&inner), panes, primary_window_id)?;
                if !incarnation_is_current() {
                    bail!("client attachment retired during coherent topology application");
                }
                Ok(true)
            })
            .await?;
        if !topology_current || !incarnation_is_current() {
            return Ok(false);
        }

        rpc.commit_sync(RpcConsumerKind::RenderBootstrap, || {
            if !incarnation_is_current() {
                bail!("client attachment retired before render bootstrap preparation");
            }
            Self::prepare_render_application_bootstrap(mux.as_ref(), inner.as_ref(), rpc)?;
            if !incarnation_is_current() {
                bail!("client attachment retired during render bootstrap preparation");
            }
            Ok(true)
        })
        .map_err(anyhow::Error::new)?
    }

    fn prepare_render_application_bootstrap(
        mux: &Mux,
        inner: &ClientInner,
        rpc: &RpcGenerationScope,
    ) -> anyhow::Result<()> {
        for pane in mux.iter_panes() {
            let Some(client_pane) = pane.downcast_ref::<ClientPane>() else {
                continue;
            };
            if client_pane.belongs_to_client(inner) {
                client_pane.prepare_render_application_bootstrap(rpc)?;
            }
        }
        Ok(())
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

    fn exact_remote_pane_id(
        guard: &PaneOperationGuard,
        inner: &Arc<ClientInner>,
        role: &str,
    ) -> anyhow::Result<PaneId> {
        guard.with_pane(|pane| {
            let pane = pane
                .downcast_ref::<ClientPane>()
                .ok_or_else(|| anyhow!("{role} pane_id {} is not a ClientPane", guard.pane_id()))?;
            if !pane.belongs_to_client(inner) {
                bail!(
                    "{role} pane_id {} belongs to a different client attachment",
                    guard.pane_id()
                );
            }
            Ok(pane.remote_pane_id())
        })
    }

    async fn split_exact(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        moved: Option<&PaneOperationGuard>,
        split_request: SplitRequest,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(mux),
            "split target belongs to another mux registration"
        );
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        self.ensure_mux_owner(mux)?;

        let remote_target = Self::exact_remote_pane_id(target, &inner, "target")?;
        let remote_move = moved
            .map(|source| {
                anyhow::ensure!(
                    source.belongs_to(mux),
                    "split source belongs to another mux registration"
                );
                anyhow::ensure!(
                    !target.same_registration(source),
                    "cannot move pane {} into a split of itself",
                    target.pane_id()
                );
                Self::exact_remote_pane_id(source, &inner, "move source")
            })
            .transpose()?;
        let target_config = target.with_pane(|pane| pane.get_config());

        let rpc = inner.client.rpc_scope();
        let result = rpc
            .split_pane(SplitPane {
                domain: SpawnTabDomain::CurrentPaneDomain,
                pane_id: remote_target,
                split_request,
                command,
                command_dir,
                move_pane_id: remote_move,
            })
            .await?;
        if !Self::sync_remote_topology(Arc::clone(mux), self, Arc::clone(&inner), &rpc, None)
            .await?
        {
            bail!("client attachment retired while resolving split pane");
        }

        let size = result.size;
        rpc.commit_sync(RpcConsumerKind::SplitResolution, || {
            let (tab, pane, window_id) = Self::resolve_remote_spawn_entities(mux, &inner, result)?;
            if let Some(source) = moved {
                anyhow::ensure!(
                    source.is_same_pane(&pane),
                    "remote moved split resolved to a different local pane registration"
                );
            }
            if let Some(config) = target_config {
                pane.set_config(config);
            }
            target.capture_split_receipt(pane, tab, window_id, size)
        })
        .map_err(anyhow::Error::new)?
    }

    /// Apply the pane/tree portion of a validated ordered snapshot without
    /// reconstructing recursive `PaneNode` transfer trees.
    ///
    /// IDs 86-90 remain rejected by the ordinary client transport. This seam
    /// is deliberately non-dispatched until the ordered-window capability has
    /// a complete connection-generation state machine and an atomic local
    /// window-order mirror primitive.
    #[allow(dead_code)]
    pub(crate) fn process_pane_arena(
        mux: &Arc<Mux>,
        inner: Arc<ClientInner>,
        panes: PaneArena,
        primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        let preflight = preflight_pane_arena(&panes)?;
        ensure_pane_arena_append_order_is_sound(
            mux,
            &inner,
            &preflight.tabs,
            &preflight.remote_pane_tabs,
            &preflight.window_ids,
        )?;
        if primary_window_id.is_some()
            && preflight.tabs.iter().any(|plan| {
                inner
                    .remote_to_local_window(plan.remote_window_id)
                    .is_none_or(|local_window_id| mux.get_window(local_window_id).is_none())
            })
        {
            bail!(
                "ordered pane arena requires transactional primary-window reuse before it can \
                 bootstrap an unmapped remote window"
            );
        }

        let mut reserved_local_pane_ids =
            inner
                .reserve_local_pane_ids(preflight.remote_pane_ids)
                .context("reserve local pane identifiers for ordered remote topology")?;
        let live_panes = mux.iter_panes();
        let mut local_pane_ids_by_remote = HashMap::new();
        local_pane_ids_by_remote
            .try_reserve(live_panes.len())
            .context("reserve ordered pane live-pane index")?;
        for pane in live_panes {
            if pane.domain_id() != inner.local_domain_id {
                continue;
            }
            if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                if !client_pane.belongs_to_client(&inner) {
                    continue;
                }
                index_live_client_pane(
                    &mut local_pane_ids_by_remote,
                    client_pane.remote_pane_id(),
                    pane.pane_id(),
                )?;
            }
        }
        let mut remote_windows_to_forget = HashSet::new();
        {
            let mappings = lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window");
            remote_windows_to_forget
                .try_reserve(mappings.len())
                .context("reserve ordered pane stale-window marks")?;
            remote_windows_to_forget.extend(mappings.keys().copied());
        }
        let mut remote_tabs_to_forget = HashSet::new();
        {
            let mappings = lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab");
            remote_tabs_to_forget
                .try_reserve(mappings.len())
                .context("reserve ordered pane stale-tab marks")?;
            remote_tabs_to_forget.extend(mappings.keys().copied());
        }
        let mut remote_panes_to_forget = HashSet::new();
        {
            let mappings = lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane");
            remote_panes_to_forget
                .try_reserve(mappings.len())
                .context("reserve ordered pane stale-pane marks")?;
            remote_panes_to_forget.extend(mappings.keys().copied());
        }

        let (descriptors, mut nodes, window_titles) = panes.into_parts();
        if descriptors.len() != preflight.tabs.len() {
            bail!("ordered pane arena changed descriptor cardinality after preflight");
        }
        if window_titles.len() != preflight.window_ids.len() {
            bail!("ordered pane arena changed window-title cardinality after preflight");
        }
        let mut prepared_tabs = Vec::new();
        prepared_tabs
            .try_reserve_exact(descriptors.len())
            .context("reserve prepared ordered pane tabs")?;
        let mut pending = PendingPaneArenaPublication::default();
        pending
            .new_panes
            .try_reserve_exact(reserved_local_pane_ids.by_remote_pane.len())
            .context("reserve pending ordered pane registrations")?;
        pending
            .existing_sync
            .try_reserve_exact(local_pane_ids_by_remote.len())
            .context("reserve pending ordered pane state updates")?;
        let mut preparation_scratch = PaneArenaPreparationScratch::default();
        for (descriptor, plan) in descriptors.into_iter().zip(preflight.tabs).rev() {
            if usize::try_from(descriptor.node_count).ok() != Some(plan.node_count) {
                bail!("ordered pane arena descriptor changed after preflight");
            }
            let mut workspace = None;
            let tree = prepare_pane_tree_from_arena_with_scratch(
                &mut nodes,
                plan.node_count,
                &mut preparation_scratch,
                |mut entry| {
                    if workspace.is_none() {
                        workspace = Some(std::mem::take(&mut entry.workspace));
                    }
                    resolve_pane_arena_entry(
                        mux,
                        &inner,
                        entry,
                        &mut remote_panes_to_forget,
                        &mut local_pane_ids_by_remote,
                        &mut reserved_local_pane_ids,
                        &mut pending,
                    )
                },
            )?;
            let workspace = workspace.ok_or_else(|| {
                anyhow!(
                    "ordered pane arena tab {} lost its preflighted workspace authority",
                    plan.remote_tab_id
                )
            })?;
            prepared_tabs.push(PreparedPaneArenaTab {
                plan,
                workspace,
                tab_title: descriptor.tab_title,
                tree,
            });
        }
        if !nodes.is_empty() {
            bail!(
                "ordered pane arena retained {} nodes after direct preparation",
                nodes.len()
            );
        }
        drop(nodes);
        drop(preparation_scratch);
        prepared_tabs.reverse();

        let mut publication = PaneArenaPublicationRollback::new(mux);
        publication
            .pane_registrations
            .try_reserve_exact(pending.new_panes.len())
            .context("reserve ordered pane registration rollback authority")?;
        for (remote_pane_id, pane) in &pending.new_panes {
            mux.add_pane(pane)
                .with_context(|| format!("register remote pane {remote_pane_id} in mux"))?;
            let registration = mux.capture_pane_registration(pane).ok_or_else(|| {
                anyhow!(
                    "remote pane {remote_pane_id} was published without exact rollback authority"
                )
            })?;
            publication.pane_registrations.push(registration);
        }

        let mut local_tabs_by_remote = HashMap::new();
        local_tabs_by_remote
            .try_reserve(prepared_tabs.len())
            .context("reserve ordered pane staged tab identities")?;
        let mut staged_tabs = Vec::new();
        staged_tabs
            .try_reserve_exact(prepared_tabs.len())
            .context("reserve ordered pane staged tabs")?;
        let existing_tab_mappings = {
            let mappings = lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab");
            let mut snapshot = HashMap::new();
            snapshot
                .try_reserve(mappings.len())
                .context("reserve ordered pane existing tab mappings")?;
            snapshot.extend(mappings.iter().map(|(remote, local)| (*remote, *local)));
            snapshot
        };
        publication
            .new_tabs
            .try_reserve_exact(prepared_tabs.len())
            .context("reserve ordered pane tab rollback authority")?;
        publication
            .new_windows
            .try_reserve_exact(preflight.window_ids.len())
            .context("reserve ordered pane window rollback authority")?;
        for prepared in prepared_tabs {
            let remote_tab_id = prepared.plan.remote_tab_id;
            let tab = existing_tab_mappings
                .get(&remote_tab_id)
                .copied()
                .and_then(|local_tab_id| mux.get_tab(local_tab_id))
                .unwrap_or_else(|| Arc::new(Tab::new(&prepared.plan.root_size)));
            if mux.get_tab(tab.tab_id()).is_none() {
                mux.add_tab_no_panes(&tab)
                    .with_context(|| format!("stage ordered remote tab {remote_tab_id} in mux"))?;
                publication.new_tabs.push(Arc::clone(&tab));
            }
            local_tabs_by_remote.insert(remote_tab_id, Arc::clone(&tab));
            staged_tabs.push(StagedPaneArenaTab { prepared, tab });
        }

        let mut local_windows_by_remote = HashMap::new();
        local_windows_by_remote
            .try_reserve(preflight.window_ids.len())
            .context("reserve ordered pane staged window identities")?;
        let existing_window_mappings = {
            let mappings = lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window");
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve_exact(mappings.len())
                .context("reserve ordered pane existing window mappings")?;
            snapshot.extend(mappings.iter().map(|(remote, local)| (*remote, *local)));
            snapshot
        };
        for (remote_window_id, local_window_id) in existing_window_mappings {
            if mux.get_window(local_window_id).is_some() {
                local_windows_by_remote.insert(remote_window_id, local_window_id);
            }
        }
        let mut attached_tabs = HashSet::new();
        attached_tabs
            .try_reserve(
                existing_tab_mappings
                    .len()
                    .saturating_add(staged_tabs.len()),
            )
            .context("reserve ordered pane attachment index")?;
        for &local_window_id in local_windows_by_remote.values() {
            let window = mux.get_window(local_window_id).ok_or_else(|| {
                anyhow!(
                    "local window {local_window_id} disappeared while indexing ordered \
                     attachments"
                )
            })?;
            attached_tabs.extend(
                window
                    .iter()
                    .map(|attached_tab| (local_window_id, attached_tab.tab_id())),
            );
        }

        for staged in &mut staged_tabs {
            let plan = &staged.prepared.plan;
            remote_windows_to_forget.remove(&plan.remote_window_id);
            remote_tabs_to_forget.remove(&plan.remote_tab_id);

            if let Some(local_window_id) =
                local_windows_by_remote.get(&plan.remote_window_id).copied()
            {
                if attached_tabs.insert((local_window_id, staged.tab.tab_id())) {
                    mux.add_tab_to_window(&staged.tab, local_window_id)
                        .with_context(|| {
                            format!(
                                "attach ordered remote tab {} to existing local window {}",
                                plan.remote_tab_id, local_window_id
                            )
                        })?;
                }
                continue;
            }

            let window_builder =
                mux.new_empty_window(Some(std::mem::take(&mut staged.prepared.workspace)), None);
            let local_window_id = *window_builder;
            publication.new_windows.push(window_builder);
            local_windows_by_remote.insert(plan.remote_window_id, local_window_id);
            mux.add_tab_to_window(&staged.tab, local_window_id)
                .with_context(|| {
                    format!(
                        "attach ordered remote tab {} to staged local window {}",
                        plan.remote_tab_id, local_window_id
                    )
                })?;
            attached_tabs.insert((local_window_id, staged.tab.tab_id()));
        }

        for StagedPaneArenaTab { prepared, tab } in staged_tabs {
            mux.set_tab_title(tab.tab_id(), &prepared.tab_title);
            tab.sync_with_prepared_pane_tree(prepared.plan.root_size, prepared.tree);
        }

        for (pane, alt_screen_active) in pending.existing_sync {
            if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                client_pane.sync_remote_listing_state(alt_screen_active);
            }
        }
        {
            let mut pane_mappings =
                lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane");
            pane_mappings.extend(
                local_pane_ids_by_remote
                    .iter()
                    .map(|(remote, local)| (*remote, *local)),
            );
        }
        {
            let mut tab_mappings =
                lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab");
            tab_mappings.extend(
                local_tabs_by_remote
                    .iter()
                    .map(|(remote, tab)| (*remote, tab.tab_id())),
            );
        }
        {
            let mut window_mappings =
                lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window");
            window_mappings.extend(
                local_windows_by_remote
                    .iter()
                    .map(|(remote, local)| (*remote, *local)),
            );
        }
        publication.commit();

        for (window_title, remote_window_id) in window_titles.into_iter().zip(preflight.window_ids)
        {
            remote_windows_to_forget.remove(&remote_window_id);
            if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                if let Some(mut window) = mux.get_window_mut(local_window_id) {
                    window.set_title(&window_title.title);
                } else {
                    lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window")
                        .remove(&remote_window_id);
                }
            }
        }

        if !remote_windows_to_forget.is_empty() {
            let mut windows =
                lock_or_recover(&inner.remote_to_local_window, "remote_to_local_window");
            for remote_window_id in remote_windows_to_forget {
                windows.remove(&remote_window_id);
            }
        }
        if !remote_tabs_to_forget.is_empty() {
            let mut tabs = lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab");
            for remote_tab_id in remote_tabs_to_forget {
                tabs.remove(&remote_tab_id);
            }
        }
        if !remote_panes_to_forget.is_empty() {
            let mut panes = lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane");
            for remote_pane_id in remote_panes_to_forget {
                panes.remove(&remote_pane_id);
            }
        }

        Ok(())
    }

    fn process_pane_list(
        mux: &Arc<Mux>,
        inner: Arc<ClientInner>,
        panes: ListPanesResponse,
        mut primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        panes
            .validate_floating_panes()
            .context("validating bounded floating-pane snapshot")?;
        if panes.tabs.len() != panes.tab_titles.len() {
            bail!(
                "malformed ListPanes response: {} tab tree(s) but {} tab title(s); refusing \
                 identifier reservation or topology mutation",
                panes.tabs.len(),
                panes.tab_titles.len()
            );
        }
        log::debug!(
            "domain {}: ListPanes snapshot has {} tab trees and {} tab titles",
            inner.local_domain_id,
            panes.tabs.len(),
            panes.tab_titles.len()
        );

        // Check out one fallback local identifier for every unique remote pane
        // before publishing any tabs, panes, or windows. This remains safe if
        // a pane from the live snapshot disappears during the tree walk. IDs
        // that are not consumed are returned to the per-domain spare bank, so
        // stable large-session resyncs do not burn through the process-wide
        // PaneId namespace.
        let mut remote_pane_ids = Vec::new();
        let mut seen_remote_pane_ids = HashSet::new();
        let mut remote_tab_owners = HashMap::new();
        let mut remote_pane_tabs = HashMap::new();
        for tabroot in &panes.tabs {
            let mut tree_identity = None;
            collect_remote_pane_ids(
                tabroot,
                &mut tree_identity,
                &mut seen_remote_pane_ids,
                &mut remote_pane_ids,
                &mut remote_pane_tabs,
            )?;
            if let Some((window_id, tab_id)) = tree_identity {
                if remote_tab_owners.insert(tab_id, window_id).is_some() {
                    bail!(
                        "malformed ListPanes response: remote tab {tab_id} appears in more than \
                         one tree"
                    );
                }
            }
        }
        for floating in &panes.floating_panes {
            let entry = &floating.pane;
            let Some(expected_window_id) = remote_tab_owners.get(&entry.tab_id).copied() else {
                bail!(
                    "malformed ListPanes response: floating pane {} names absent remote tab {}",
                    entry.pane_id,
                    entry.tab_id
                );
            };
            if expected_window_id != entry.window_id {
                bail!(
                    "malformed ListPanes response: floating pane {} names window/tab {}/{}, but \
                     the tab tree belongs to window {}",
                    entry.pane_id,
                    entry.window_id,
                    entry.tab_id,
                    expected_window_id
                );
            }
            if !seen_remote_pane_ids.insert(entry.pane_id) {
                bail!(
                    "malformed ListPanes response: remote pane {} has more than one tiled/floating owner",
                    entry.pane_id
                );
            }
            if remote_pane_tabs
                .insert(entry.pane_id, entry.tab_id)
                .is_some()
            {
                bail!(
                    "malformed ListPanes response: floating pane {} has conflicting tab owners",
                    entry.pane_id
                );
            }
            if entry.is_active_pane != floating.focused || entry.is_zoomed_pane {
                bail!(
                    "malformed ListPanes response: floating pane {} carries contradictory focus/zoom metadata",
                    entry.pane_id
                );
            }
            if entry.left_col != floating.rect.left
                || entry.top_row != floating.rect.top
                || entry.size.cols != floating.rect.width
                || entry.size.rows != floating.rect.height
            {
                bail!(
                    "malformed ListPanes response: floating pane {} geometry disagrees with its pane entry",
                    entry.pane_id
                );
            }
            remote_pane_ids.push(entry.pane_id);
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
                if let Some(expected_remote_tab_id) = remote_pane_tabs.get(&remote_pane_id).copied()
                {
                    if client_pane.remote_tab_id != expected_remote_tab_id {
                        bail!(
                            "remote pane {remote_pane_id} moved from tab {} to tab \
                             {expected_remote_tab_id}; atomic pane migration is required",
                            client_pane.remote_tab_id
                        );
                    }
                }
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

        let mut local_tabs_by_remote = HashMap::new();
        local_tabs_by_remote
            .try_reserve(remote_tab_owners.len())
            .context("reserve local tab identities for floating-pane reconciliation")?;
        let mut authoritative_panes_by_remote = HashMap::new();
        authoritative_panes_by_remote
            .try_reserve(seen_remote_pane_ids.len())
            .context("reserve authoritative pane identities for floating reconciliation")?;
        let mut pending_tiled_sync = Vec::new();
        pending_tiled_sync
            .try_reserve_exact(seen_remote_pane_ids.len())
            .context("reserve pending tiled-pane state updates")?;

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

                if local_tabs_by_remote
                    .insert(remote_tab_id, Arc::clone(&tab))
                    .is_some()
                {
                    bail!(
                        "malformed ListPanes response: remote tab {remote_tab_id} resolved more than once"
                    );
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
                    if pane.downcast_ref::<ClientPane>().is_some() {
                        pending_tiled_sync.push((Arc::clone(&pane), entry.alt_screen_active));
                    }
                    if authoritative_panes_by_remote
                        .insert(entry.pane_id, Arc::clone(&pane))
                        .is_some()
                    {
                        bail!(
                            "malformed ListPanes response: remote pane {} resolved more than once",
                            entry.pane_id
                        );
                    }
                    Ok(pane)
                })?;

                if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                    let needs_attach = mux
                        .get_window(local_window_id)
                        .map(|window| window.iter().all(|candidate| !Arc::ptr_eq(candidate, &tab)));
                    if let Some(needs_attach) = needs_attach {
                        if needs_attach {
                            log::debug!(
                                "domain: {} adding tab to existing local window {}",
                                inner.local_domain_id,
                                local_window_id
                            );
                            mux.add_tab_to_window(&tab, local_window_id)
                                .with_context(|| {
                                    format!(
                                        "attach remote tab {} to existing local window {}",
                                        tab.tab_id(),
                                        local_window_id
                                    )
                                })?;
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

        let mut desired_floating = Vec::new();
        desired_floating
            .try_reserve_exact(panes.floating_panes.len())
            .context("reserve authoritative floating-pane states")?;
        let mut pending_float_mappings = PendingFloatingPaneMappings::new(
            &mut reserved_local_pane_ids,
            panes.floating_panes.len(),
        )?;
        let mut pending_float_sync = Vec::new();
        pending_float_sync
            .try_reserve_exact(panes.floating_panes.len())
            .context("reserve pending floating-pane state updates")?;

        for floating in panes.floating_panes {
            let entry = floating.pane;
            remote_panes_to_forget.remove(&entry.pane_id);
            let tab = local_tabs_by_remote
                .get(&entry.tab_id)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "floating pane {} lost its local tab mapping for remote tab {}",
                        entry.pane_id,
                        entry.tab_id
                    )
                })?;

            let pane = if let Some(local_pane_id) =
                local_pane_ids_by_remote.get(&entry.pane_id).copied()
            {
                match mux.get_pane(local_pane_id) {
                    Some(pane)
                        if pane
                            .downcast_ref::<ClientPane>()
                            .is_some_and(|client_pane| {
                                client_pane.belongs_to_client(&inner)
                                    && client_pane.remote_pane_id() == entry.pane_id
                                    && client_pane.remote_tab_id == entry.tab_id
                            }) =>
                    {
                        pending_float_sync.push((Arc::clone(&pane), entry.alt_screen_active));
                        pane
                    }
                    Some(_) | None => {
                        let local_pane_id =
                            pending_float_mappings.take(entry.pane_id).ok_or_else(|| {
                                anyhow!(
                                    "remote floating pane {} needs a local identifier, but no \
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
                        local_pane_ids_by_remote.insert(entry.pane_id, local_pane_id);
                        pane
                    }
                }
            } else {
                let local_pane_id =
                    pending_float_mappings.take(entry.pane_id).ok_or_else(|| {
                        anyhow!(
                            "remote floating pane {} needs a local identifier, but no identifier \
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
                local_pane_ids_by_remote.insert(entry.pane_id, local_pane_id);
                pane
            };

            if authoritative_panes_by_remote
                .insert(entry.pane_id, Arc::clone(&pane))
                .is_some()
            {
                bail!(
                    "malformed ListPanes response: remote floating pane {} resolved more than once",
                    entry.pane_id
                );
            }
            desired_floating.push(DomainFloatingPaneState {
                tab,
                pane,
                pane_id: local_pane_ids_by_remote[&entry.pane_id],
                rect: floating.rect,
                z_order: floating.z_order,
                visible: floating.visible,
                pinned: floating.pinned,
                opacity: floating.opacity,
                focused: floating.focused,
            });
        }

        if authoritative_panes_by_remote.len() != seen_remote_pane_ids.len() {
            bail!(
                "ListPanes resolved {} of {} authoritative panes",
                authoritative_panes_by_remote.len(),
                seen_remote_pane_ids.len()
            );
        }
        let mut authoritative_panes = Vec::new();
        authoritative_panes
            .try_reserve_exact(authoritative_panes_by_remote.len())
            .context("reserve authoritative local pane set")?;
        authoritative_panes.extend(authoritative_panes_by_remote.into_values());

        let reconcile_receipt = mux
            .reconcile_domain_floating_panes(
                inner.local_domain_id,
                authoritative_panes,
                desired_floating,
            )
            .context("reconcile authoritative floating-pane topology")?;
        let mut pending_float_mappings = pending_float_mappings.commit();
        pending_float_mappings.sort_unstable_by_key(|(_, local_pane_id)| *local_pane_id);
        debug_assert_eq!(
            pending_float_mappings.len(),
            reconcile_receipt.registered_pane_ids.len()
        );
        debug_assert!(pending_float_mappings
            .iter()
            .map(|(_, local_pane_id)| *local_pane_id)
            .eq(reconcile_receipt.registered_pane_ids.iter().copied()));

        for (pane, alt_screen_active) in pending_tiled_sync {
            if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                client_pane.sync_remote_listing_state(alt_screen_active);
            }
        }
        for (pane, alt_screen_active) in pending_float_sync {
            if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                client_pane.sync_remote_listing_state(alt_screen_active);
            }
        }
        {
            let mut pane_mappings =
                lock_or_recover(&inner.remote_to_local_pane, "remote_to_local_pane");
            for (remote_pane_id, local_pane_id) in &pending_float_mappings {
                pane_mappings.insert(*remote_pane_id, *local_pane_id);
            }
        }
        log::debug!(
            "domain {} floating reconciliation changed {} tab(s), registered {} pane(s), and \
             retired {} pane(s)",
            inner.local_domain_id,
            reconcile_receipt.changed_tab_ids.len(),
            reconcile_receipt.registered_pane_ids.len(),
            reconcile_receipt.retired_pane_ids.len(),
        );

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

    async fn finish_attach(
        mux: &Arc<Mux>,
        domain_id: DomainId,
        client: Client,
        rpc: RpcGenerationScope,
        readiness_guard: &RpcGenerationAbortGuard,
        request: InitialAttachmentRequest,
    ) -> anyhow::Result<()> {
        let InitialAttachmentRequest {
            owner_client_id,
            primary_window_id,
        } = request;
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
        let mut cleanup = InitialAttachmentCleanup {
            mux: Arc::clone(mux),
            domain_registration: Arc::clone(&domain_registration),
            inner: Arc::clone(&inner),
            rpc: rpc.clone(),
            armed: true,
        };

        rpc.with_coherent_topology_snapshot(RpcConsumerKind::InitialAttachment, |panes| {
            // Process the pane list BEFORE publishing inner to the domain.
            // This prevents concurrent operations from seeing a partially
            // attached domain with incomplete pane mappings. The pending claim
            // rejects a second initial attachment without holding a callback-
            // reentrant mutex across mux topology mutation.
            let result = (|| {
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
                        "client domain {domain_id} owner client registration retired during \
                         attachment preparation"
                    );
                }
                *published = Some(Arc::clone(&inner));
                anyhow::Result::<()>::Ok(())
            })();
            if result.is_err() {
                // Run cleanup while the exact generation's consumer lease is
                // still held. This prevents successor publication between a
                // partial topology mutation and its rollback.
                cleanup.cleanup_if_current();
            }
            result
        })
        .await?;

        rpc.commit_sync(RpcConsumerKind::RenderBootstrap, || {
            if inner.is_detached()
                || domain.retired.load(Ordering::Acquire)
                || !domain.inner_is_current(&inner)
                || !mux
                    .get_domain(domain_id)
                    .is_some_and(|current| Arc::ptr_eq(&current, &domain_registration))
            {
                bail!("client domain {domain_id} retired before initial render bootstrap");
            }
            Self::prepare_render_application_bootstrap(mux.as_ref(), inner.as_ref(), &rpc)?;
            if inner.is_detached()
                || domain.retired.load(Ordering::Acquire)
                || !domain.inner_is_current(&inner)
                || !mux
                    .get_domain(domain_id)
                    .is_some_and(|current| Arc::ptr_eq(&current, &domain_registration))
            {
                bail!("client domain {domain_id} retired during initial render bootstrap");
            }
            Ok(())
        })
        .map_err(anyhow::Error::new)??;

        let bootstrap_result = async {
            if let Some(request) = current_active_workspace_sync(&inner, mux) {
                rpc.set_active_workspace(request)
                    .await
                    .context("synchronizing the initial active workspace")?;
            }
            inner
                .client
                .publish_rpc_transport_ready(&rpc, readiness_guard)
                .await
                .context("publishing initial mux RPC readiness")
        }
        .await;
        bootstrap_result?;
        cleanup.disarm();

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

    fn supports_floating_pane_spawn(&self) -> bool {
        // A client-domain spawn is authoritative only on the remote mux. The
        // current floating-pane PDUs move already-existing panes and do not
        // combine spawn, source detachment, destination attachment, and tab
        // retirement. Refuse before sending SpawnV2 until that transaction is
        // represented by one remote operation.
        false
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

        if !Self::sync_remote_topology(Arc::clone(mux), self, Arc::clone(&inner), &rpc, None)
            .await?
        {
            bail!("client attachment retired while resolving spawned pane");
        }
        rpc.commit_sync(RpcConsumerKind::SpawnResolution, || {
            let (_tab, pane, _window_id) =
                Self::resolve_remote_spawn_entities(mux, &inner, result)?;
            Ok(pane)
        })
        .map_err(anyhow::Error::new)?
    }

    /// Forward the request to the remote; we need to translate the local ids
    /// to those that match the remote for the request, resync the changed
    /// structure, and then translate the results back to local
    async fn move_pane_to_new_tab(
        &self,
        mux: &Arc<Mux>,
        pane_guard: &PaneOperationGuard,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<Option<MoveCommitReceipt>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        self.ensure_mux_owner(mux)?;
        anyhow::ensure!(
            pane_guard.belongs_to(mux),
            "move target belongs to another mux registration"
        );
        let remote_pane_id = Self::exact_remote_pane_id(pane_guard, &inner, "move target")?;

        let remote_window_id =
            window_id.and_then(|local_window| inner.local_to_remote_window(local_window));

        let rpc = inner.client.rpc_scope();
        let result = rpc
            .move_pane_to_new_tab(codec::MovePaneToNewTab {
                pane_id: remote_pane_id,
                window_id: remote_window_id,
                workspace_for_new_window,
            })
            .await?;

        if !Self::sync_remote_topology(Arc::clone(mux), self, Arc::clone(&inner), &rpc, None)
            .await?
        {
            bail!("client attachment retired while moving pane");
        }

        rpc.commit_sync(RpcConsumerKind::MoveResolution, || {
            let local_tab_id = inner.remote_to_local_tab_id(result.tab_id).ok_or_else(|| {
                anyhow!("remote tab {} didn't resolve after resync", result.tab_id)
            })?;

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

            pane_guard.capture_move_receipt(tab, local_win_id).map(Some)
        })
        .map_err(anyhow::Error::new)?
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
        rpc.commit_sync(RpcConsumerKind::SpawnResolution, || {
            let (tab, _pane, _window_id) =
                Self::resolve_remote_spawn_entities(mux, &inner, result)?;
            Ok(tab)
        })
        .map_err(anyhow::Error::new)?
    }

    async fn split_pane_spawned(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        split_request: SplitRequest,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<SplitCommitReceipt> {
        self.split_exact(mux, target, None, split_request, command, command_dir)
            .await
    }

    async fn split_pane_moved(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        source: &PaneOperationGuard,
        split_request: SplitRequest,
    ) -> anyhow::Result<SplitCommitReceipt> {
        self.split_exact(mux, target, Some(source), split_request, None, None)
            .await
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
                let _ = Self::sync_remote_topology(Arc::clone(mux), self, inner, &rpc, window_id)
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
                let rpc = client.bootstrap_rpc_scope();
                let mut abort_guard = rpc
                    .abort_guard("initial mux RPC bootstrap failed, timed out, or was cancelled")?;
                let result = with_mux_rpc_bootstrap_timeout(async {
                    client.verify_version_compat_with_scope(&ui, &rpc).await?;

                    ui.output_str("Version check OK!  Requesting coherent topology snapshot...\n");
                    ClientDomain::finish_attach(
                        &mux,
                        domain_id,
                        client,
                        rpc,
                        &abort_guard,
                        InitialAttachmentRequest {
                            owner_client_id,
                            primary_window_id: window_id,
                        },
                    )
                    .await
                })
                .await;
                if result.is_ok() {
                    abort_guard.disarm();
                }
                result
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
    fn exact_id_reverse_lookup_work_is_q_linear_at_large_tab_counts() {
        for count in [1_024usize, 4_096, 16_384] {
            let mut mappings = ExactIdMappings::<TabId, TabId>::default();
            for remote in 0..count {
                assert_eq!(mappings.insert(remote, count + remote), None);
            }
            for local in count..count.saturating_mul(2) {
                assert_eq!(mappings.get_remote(&local), Some(&(local - count)));
            }
            assert_eq!(
                mappings.reverse_lookup_probes(),
                count,
                "one reverse lookup must perform one indexed probe regardless of map size",
            );
            eprintln!(
                "client_reverse_mapping_work tab_count={count} lookups={count} hash_probes={}",
                mappings.reverse_lookup_probes(),
            );
        }
    }

    #[test]
    fn exact_id_mapping_reassignment_preserves_one_to_one_reverse_authority() {
        let mut mappings = ExactIdMappings::<TabId, TabId>::default();
        mappings.extend([(1, 101), (2, 102)]);

        assert_eq!(mappings.insert(1, 103), Some(101));
        assert_eq!(mappings.get(&1), Some(&103));
        assert_eq!(mappings.get_remote(&101), None);
        assert_eq!(mappings.get_remote(&103), Some(&1));

        assert_eq!(mappings.insert(3, 102), None);
        assert_eq!(mappings.get(&2), None);
        assert_eq!(mappings.get(&3), Some(&102));
        assert_eq!(mappings.get_remote(&102), Some(&3));

        mappings.retain(|remote, _local| *remote != 1);
        assert_eq!(mappings.get(&1), None);
        assert_eq!(mappings.get_remote(&103), None);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings.remove(&3), Some(102));
        assert_eq!(mappings.get_remote(&102), None);
        assert!(mappings.is_empty());
    }

    #[test]
    fn exact_id_idempotent_insert_repairs_a_torn_forward_alias() {
        let mut mappings = ExactIdMappings::<TabId, TabId>::default();
        mappings.insert(1, 101);
        mappings.insert_forward_alias_for_test(2, 101);

        assert_eq!(mappings.insert(2, 101), Some(101));
        assert_eq!(mappings.get(&1), None);
        assert_eq!(mappings.get(&2), Some(&101));
        assert_eq!(mappings.get_remote(&101), Some(&2));
        assert_eq!(mappings.len(), 1);
    }

    #[test]
    fn exact_id_retain_predicate_panic_leaves_bijection_unchanged() {
        let mut mappings = ExactIdMappings::<TabId, TabId>::default();
        mappings.extend([(1, 101), (2, 102), (3, 103)]);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            mappings.retain(|remote, _local| {
                assert_ne!(*remote, 2, "injected retain predicate panic");
                *remote != 1
            });
        }));
        assert!(result.is_err());
        for remote in 1..=3 {
            let local = 100 + remote;
            assert_eq!(mappings.get(&remote), Some(&local));
            assert_eq!(mappings.get_remote(&local), Some(&remote));
        }
        assert_eq!(mappings.len(), 3);
    }

    fn register_test_client_domain(mux: &Arc<Mux>, inner: &Arc<ClientInner>) -> Arc<ClientDomain> {
        let config = ClientDomainConfig::Unix(UnixDomain {
            name: format!("test-client-domain-{}", inner.local_domain_id),
            ..UnixDomain::default()
        });
        let domain = Arc::new(ClientDomain {
            label: config.label(),
            config,
            inner: Mutex::new(Some(Arc::clone(inner))),
            initial_attachment_pending: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            local_domain_id: inner.local_domain_id,
            mux_owner: Arc::downgrade(mux),
            mux_subscriber_id: None,
        });
        let registered: Arc<dyn Domain> = domain.clone();
        mux.add_domain(&registered)
            .expect("test client domain should register with its exact mux");
        domain
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
            floating_panes: Vec::new(),
        }
    }

    fn sample_remote_tab_listing_with_float() -> ListPanesResponse {
        let mut listing = sample_remote_tab_listing();
        let PaneNode::Leaf(template) = listing.tabs[0].clone() else {
            panic!("sample remote tab must contain one pane leaf");
        };
        let mut pane = template;
        pane.pane_id = 62;
        pane.title = "remote floating shell".to_string();
        pane.size = TerminalSize {
            cols: 20,
            rows: 8,
            pixel_width: 200,
            pixel_height: 160,
            dpi: 96,
        };
        pane.alt_screen_active = false;
        pane.is_active_pane = false;
        pane.is_zoomed_pane = false;
        pane.left_col = 4;
        pane.top_row = 3;
        listing
            .floating_panes
            .push(codec::FloatingPaneSnapshotEntry {
                pane,
                rect: mux::tab::FloatingPaneRect {
                    left: 4,
                    top: 3,
                    width: 20,
                    height: 8,
                },
                z_order: 7,
                visible: true,
                pinned: true,
                opacity: 0.75,
                focused: false,
            });
        listing
    }

    fn sample_remote_pane_arena(tab_and_pane_ids: &[(TabId, PaneId)]) -> PaneArena {
        let mut listing = sample_remote_tab_listing();
        let PaneNode::Leaf(template) = listing
            .tabs
            .pop()
            .expect("sample listing must contain one leaf")
        else {
            panic!("sample listing root must be a leaf");
        };
        listing.tab_titles.clear();
        for &(tab_id, pane_id) in tab_and_pane_ids {
            let mut entry = template.clone();
            entry.tab_id = tab_id;
            entry.pane_id = pane_id;
            entry.title = format!("remote shell {pane_id}");
            listing.tabs.push(PaneNode::Leaf(entry));
            listing.tab_titles.push(format!("remote tab {tab_id}"));
        }
        codec::ordered_pane_arena_from_list_panes(listing)
            .expect("sample listing must flatten into a canonical pane arena")
    }

    fn remote_tab_order(mux: &Mux, inner: &ClientInner, local_window_id: WindowId) -> Vec<TabId> {
        mux.window_order_snapshot(local_window_id)
            .expect("local window order must be valid")
            .expect("local window must exist")
            .ordered_tab_ids()
            .map(|local_tab_id| {
                inner
                    .local_to_remote_tab(local_tab_id)
                    .expect("every ordered test tab must have a remote identity")
            })
            .collect()
    }

    fn client_remote_pane_id(pane: &Arc<dyn Pane>) -> PaneId {
        pane.downcast_ref::<ClientPane>()
            .expect("ordered test pane must be a ClientPane")
            .remote_pane_id()
    }

    #[test]
    fn pane_arena_publication_rollback_removes_emptied_published_windows() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);

        let tab = Arc::new(Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("rollback fixture tab must register");
        let inner = test_client_inner(70_001);
        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
            &inner,
            70_002,
            51,
            61,
            TerminalSize::default(),
            "rollback pane",
            false,
        ));
        mux.add_pane(&pane)
            .expect("rollback fixture pane must register");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("rollback fixture must retain exact pane registration authority");
        tab.assign_pane(&pane);
        let window_builder = mux.new_empty_window(Some("rollback".to_string()), None);
        let window_id = *window_builder;
        mux.add_tab_to_window(&tab, window_id)
            .expect("rollback fixture tab must attach to provisional window");

        let mut publication = PaneArenaPublicationRollback::new(&mux);
        publication.pane_registrations.push(registration);
        publication.new_tabs.push(Arc::clone(&tab));
        publication.new_windows.push(window_builder);
        drop(publication);

        assert!(
            mux.get_window(window_id).is_none(),
            "compensating rollback must remove the already-published window it emptied"
        );
        assert!(
            mux.get_tab(tab.tab_id()).is_none(),
            "rollback must remove the exact staged tab registration"
        );
        assert!(
            mux.get_pane(pane.pane_id()).is_none(),
            "rollback must detach the populated local pane mirror"
        );
        assert!(mux.iter_windows().is_empty());
    }

    #[test]
    fn direct_pane_arena_application_preserves_forward_tab_order() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_005);

        ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            sample_remote_pane_arena(&[(51, 61), (52, 62), (53, 63)]),
            None,
        )
        .expect("direct flat arena application should attach in descriptor order");

        let local_window_id = inner
            .remote_to_local_window(41)
            .expect("remote window should map locally");
        assert_eq!(
            remote_tab_order(&mux, &inner, local_window_id),
            vec![51, 52, 53]
        );
        assert_eq!(mux.iter_panes().len(), 3);
        assert_eq!(mux.iter_windows().len(), 1);
        assert_eq!(
            mux.get_window(local_window_id)
                .expect("ordered local window must exist")
                .get_title(),
            "ops window"
        );
        for remote_tab_id in [51, 52, 53] {
            let local_tab_id = inner
                .remote_to_local_tab_id(remote_tab_id)
                .expect("remote tab should map locally");
            assert_eq!(
                mux.get_tab(local_tab_id)
                    .expect("mapped tab must exist")
                    .get_title(),
                format!("remote tab {remote_tab_id}")
            );
        }

        ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            sample_remote_pane_arena(&[(51, 61), (52, 62), (53, 63)]),
            None,
        )
        .expect("stable direct flat arena resync should reuse its mirrors");
        assert_eq!(
            remote_tab_order(&mux, &inner, local_window_id),
            vec![51, 52, 53]
        );
        assert_eq!(mux.iter_panes().len(), 3);
    }

    #[test]
    fn direct_pane_arena_application_preserves_split_shape_and_focus() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_008);
        let mut listing = sample_remote_tab_listing();
        let PaneNode::Leaf(mut left) = listing
            .tabs
            .pop()
            .expect("sample listing must contain one leaf")
        else {
            panic!("sample listing root must be a leaf");
        };
        let mut right = left.clone();
        left.pane_id = 61;
        left.is_active_pane = true;
        left.is_zoomed_pane = false;
        right.pane_id = 62;
        right.title = "remote shell 62".to_string();
        right.is_active_pane = false;
        right.is_zoomed_pane = true;
        let split = mux::tab::SplitDirectionAndSize {
            direction: mux::tab::SplitDirection::Horizontal,
            first: left.size,
            second: right.size,
        };
        listing.tabs.push(PaneNode::Split {
            left: Box::new(PaneNode::Leaf(left)),
            right: Box::new(PaneNode::Leaf(right)),
            node: split,
        });

        ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            codec::ordered_pane_arena_from_list_panes(listing)
                .expect("split listing must flatten into a canonical pane arena"),
            None,
        )
        .expect("direct flat arena application should preserve a split tree");

        let local_tab_id = inner
            .remote_to_local_tab_id(51)
            .expect("remote split tab should map locally");
        let tab = mux
            .get_tab(local_tab_id)
            .expect("mapped split tab must exist");
        let panes = tab.iter_panes_ignoring_zoom();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].top, panes[1].top);
        assert!(
            panes[1].left > panes[0].left,
            "a horizontal split must place the second pane to the right"
        );
        assert_eq!(
            tab.get_active_idx(),
            0,
            "left pane remains the base active pane"
        );
        assert_eq!(
            panes
                .iter()
                .map(|pane| client_remote_pane_id(&pane.pane))
                .collect::<Vec<_>>(),
            vec![61, 62]
        );
        assert_eq!(
            client_remote_pane_id(
                &tab.get_active_pane()
                    .expect("zoomed split pane must be publicly active")
            ),
            62,
            "the zoomed pane must retain public focus semantics"
        );
        assert_eq!(
            client_remote_pane_id(
                &tab.get_zoomed_pane()
                    .expect("split pane must retain zoom authority")
            ),
            62
        );
    }

    #[test]
    fn direct_pane_arena_rejects_reorder_before_tree_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_006);

        ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            sample_remote_pane_arena(&[(51, 61), (52, 62)]),
            None,
        )
        .expect("initial direct flat arena application should attach");
        let local_window_id = inner
            .remote_to_local_window(41)
            .expect("remote window should map locally");
        let local_first_tab = inner
            .remote_to_local_tab_id(51)
            .and_then(|tab_id| mux.get_tab(tab_id))
            .expect("first remote tab should map locally");
        let first_pane_before = local_first_tab
            .get_active_pane()
            .expect("first remote tab should have an active pane");

        let error = ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            sample_remote_pane_arena(&[(52, 62), (51, 61)]),
            None,
        )
        .expect_err("existing-window reorder must fail before pane preparation");
        assert!(
            format!("{error:#}").contains("requires an atomic existing-window reorder"),
            "unexpected error: {error:#}",
            error = error,
        );
        assert_eq!(
            remote_tab_order(&mux, &inner, local_window_id),
            vec![51, 52]
        );
        assert!(Arc::ptr_eq(
            &first_pane_before,
            &local_first_tab
                .get_active_pane()
                .expect("rejected reorder must retain the prior pane tree")
        ));
        assert_eq!(mux.iter_panes().len(), 2);
    }

    #[test]
    fn direct_pane_arena_rejects_remote_pane_migration_before_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_009);

        ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            sample_remote_pane_arena(&[(51, 61)]),
            None,
        )
        .expect("initial direct flat arena application should attach");
        let local_tab_id = inner
            .remote_to_local_tab_id(51)
            .expect("initial remote tab should map locally");
        let local_pane_id = inner
            .remote_to_local_pane_id(&mux, 61)
            .expect("initial remote pane should map locally");

        let error = ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            sample_remote_pane_arena(&[(52, 61)]),
            None,
        )
        .expect_err("moving a live remote pane between tabs must fail before mutation");
        assert!(
            format!("{error:#}").contains("atomic pane migration is required"),
            "unexpected error: {error:#}",
            error = error,
        );
        assert_eq!(inner.remote_to_local_tab_id(51), Some(local_tab_id));
        assert_eq!(inner.remote_to_local_tab_id(52), None);
        assert_eq!(inner.remote_to_local_pane_id(&mux, 61), Some(local_pane_id));
        assert_eq!(mux.iter_panes().len(), 1);
        assert_eq!(mux.iter_windows().len(), 1);
    }

    #[test]
    fn direct_pane_arena_rejects_aliased_tab_mappings_before_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_010);
        let snapshot = sample_remote_pane_arena(&[(51, 61), (52, 62)]);

        ClientDomain::process_pane_arena(&mux, Arc::clone(&inner), snapshot.clone(), None)
            .expect("initial direct flat arena application should attach");
        let first_local_tab = inner
            .remote_to_local_tab_id(51)
            .expect("first remote tab should map locally");
        lock_or_recover(&inner.remote_to_local_tab, "remote_to_local_tab")
            .insert_forward_alias_for_test(52, first_local_tab);

        let error = ClientDomain::process_pane_arena(&mux, Arc::clone(&inner), snapshot, None)
            .expect_err("aliased remote tab mappings must fail before mutation");
        assert!(
            format!("{error:#}").contains("mappings alias remote tabs"),
            "unexpected error: {error:#}",
            error = error,
        );
        assert_eq!(mux.iter_panes().len(), 2);
        assert_eq!(mux.iter_windows().len(), 1);
    }

    #[test]
    fn direct_pane_arena_rejects_foreign_tab_and_window_mappings() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let owner = test_client_inner(91_012);
        let foreign = test_client_inner(91_013);
        let snapshot = sample_remote_pane_arena(&[(51, 61)]);

        ClientDomain::process_pane_arena(&mux, Arc::clone(&owner), snapshot.clone(), None)
            .expect("owner should establish the initial direct mirror");
        let owner_tab = owner
            .remote_to_local_tab_id(51)
            .expect("owner tab should map locally");
        let owner_window = owner
            .remote_to_local_window(41)
            .expect("owner window should map locally");
        lock_or_recover(&foreign.remote_to_local_tab, "remote_to_local_tab")
            .insert_forward_alias_for_test(51, owner_tab);
        lock_or_recover(&foreign.remote_to_local_window, "remote_to_local_window")
            .insert_forward_alias_for_test(41, owner_window);

        let error = ClientDomain::process_pane_arena(&mux, Arc::clone(&foreign), snapshot, None)
            .expect_err("foreign live topology mappings must fail before mutation");
        assert!(
            format!("{error:#}").contains("does not belong exactly to this client"),
            "unexpected error: {error:#}",
            error = error,
        );
        assert_eq!(owner.remote_to_local_tab_id(51), Some(owner_tab));
        assert_eq!(owner.remote_to_local_window(41), Some(owner_window));
        assert_eq!(mux.iter_panes().len(), 1);
        assert_eq!(mux.iter_windows().len(), 1);
    }

    #[test]
    fn direct_pane_arena_rejects_stale_topology_removal_before_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_014);

        ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            sample_remote_pane_arena(&[(51, 61), (52, 62)]),
            None,
        )
        .expect("initial direct flat arena application should attach");
        let error = ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            sample_remote_pane_arena(&[(51, 61)]),
            None,
        )
        .expect_err("removing live stale topology requires an atomic reconciliation path");
        assert!(
            format!("{error:#}").contains("atomic stale-pane removal is required"),
            "unexpected error: {error:#}",
            error = error,
        );
        assert!(inner.remote_to_local_tab_id(51).is_some());
        assert!(inner.remote_to_local_tab_id(52).is_some());
        assert_eq!(mux.iter_panes().len(), 2);
        assert_eq!(mux.iter_windows().len(), 1);
    }

    #[test]
    fn direct_pane_arena_rejects_reserved_window_title_before_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_011);
        let (trees, nodes, mut window_titles) = sample_remote_pane_arena(&[(51, 61)]).into_parts();
        window_titles[0].window_id = u64::MAX;

        let error = ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&inner),
            PaneArena::from_unvalidated_parts(trees, nodes, window_titles),
            None,
        )
        .expect_err("reserved terminal identities must fail before mutation");
        assert!(
            format!("{error:#}").contains("reserved value"),
            "unexpected error: {error:#}",
            error = error,
        );
        assert!(mux.iter_panes().is_empty());
        assert!(mux.iter_windows().is_empty());
    }

    #[test]
    fn direct_pane_arena_rejects_new_empty_window_before_tree_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_007);
        let (trees, nodes, mut window_titles) = sample_remote_pane_arena(&[(51, 61)]).into_parts();
        window_titles.push(mux::tab::PaneArenaWindowTitle {
            window_id: 42,
            title: "empty remote window".to_string(),
        });
        let panes = PaneArena::from_unvalidated_parts(trees, nodes, window_titles);

        let error = ClientDomain::process_pane_arena(&mux, Arc::clone(&inner), panes, None)
            .expect_err("a new empty window without workspace authority must fail closed");
        assert!(
            format!("{error:#}")
                .contains("requires exact ordered workspace and client ownership authority"),
            "unexpected error: {error:#}",
            error = error,
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
    fn direct_pane_arena_rejects_title_only_foreign_window_mapping() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let owner = test_client_inner(91_015);
        let foreign = test_client_inner(91_016);

        ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&owner),
            sample_remote_pane_arena(&[(51, 61)]),
            None,
        )
        .expect("owner direct flat arena application should attach");
        ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&foreign),
            sample_remote_pane_arena(&[(51, 61)]),
            None,
        )
        .expect("foreign direct flat arena application should attach separately");
        let owner_window_id = owner
            .remote_to_local_window(41)
            .expect("owner remote window should map locally");
        let foreign_window_id = foreign
            .remote_to_local_window(41)
            .expect("foreign remote window should map locally");
        assert_ne!(owner_window_id, foreign_window_id);
        lock_or_recover(&owner.remote_to_local_window, "remote_to_local_window")
            .insert_forward_alias_for_test(42, foreign_window_id);

        let (trees, nodes, mut window_titles) = sample_remote_pane_arena(&[(51, 61)]).into_parts();
        window_titles.push(mux::tab::PaneArenaWindowTitle {
            window_id: 42,
            title: "must not replace owned title".to_string(),
        });
        let error = ClientDomain::process_pane_arena(
            &mux,
            Arc::clone(&owner),
            PaneArena::from_unvalidated_parts(trees, nodes, window_titles),
            None,
        )
        .expect_err("title-only mappings must not confer window ownership");
        assert!(
            format!("{error:#}")
                .contains("requires exact ordered workspace and client ownership authority"),
            "unexpected error: {error:#}",
            error = error,
        );
        assert_eq!(
            mux.get_window(foreign_window_id)
                .expect("foreign window must survive rejection")
                .get_title(),
            "ops window"
        );
        assert_eq!(mux.iter_windows().len(), 2);
        assert_eq!(mux.iter_panes().len(), 2);
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
    fn tiled_and_floating_remote_pane_alias_is_rejected_before_topology_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_017);
        let mut listing = sample_remote_tab_listing();
        let PaneNode::Leaf(entry) = listing.tabs[0].clone() else {
            panic!("sample remote tab must contain one pane leaf");
        };
        listing
            .floating_panes
            .push(codec::FloatingPaneSnapshotEntry {
                pane: entry,
                rect: mux::tab::FloatingPaneRect {
                    left: 0,
                    top: 0,
                    width: 120,
                    height: 40,
                },
                z_order: 0,
                visible: true,
                pinned: false,
                opacity: 1.0,
                focused: true,
            });

        let error = ClientDomain::process_pane_list(&mux, Arc::clone(&inner), listing, None)
            .expect_err("one remote pane cannot be both tiled and floating");

        assert!(
            error
                .to_string()
                .contains("remote pane 61 has more than one tiled/floating owner"),
            "unexpected error: {error:#}",
            error = error,
        );
        assert!(mux.iter_panes().is_empty());
        assert!(mux.iter_windows().is_empty());
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
        let _domain = register_test_client_domain(&target_mux, &inner);

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
        let _domain = register_test_client_domain(&mux, &inner);

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

    #[test]
    fn floating_snapshot_publish_replay_and_retire_preserve_exact_ownership() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(91_018);
        let _domain = register_test_client_domain(&mux, &inner);

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing_with_float(),
            None,
        )
        .expect("initial floating snapshot should attach atomically");

        let local_tab_id = inner
            .remote_to_local_tab_id(51)
            .expect("remote floating owner tab should map locally");
        let tab = mux
            .get_tab(local_tab_id)
            .expect("remote floating owner tab should be registered");
        let local_float_id = inner
            .remote_to_local_pane_id(&mux, 62)
            .expect("remote floating pane should map locally");
        let floating_pane = mux
            .get_pane(local_float_id)
            .expect("remote floating pane should publish with its owner");
        let positioned = tab.iter_floating_panes();
        assert_eq!(positioned.len(), 1);
        assert_eq!(positioned[0].pane_id, local_float_id);
        assert!(Arc::ptr_eq(&positioned[0].pane, &floating_pane));
        assert_eq!(
            (
                positioned[0].left,
                positioned[0].top,
                positioned[0].width,
                positioned[0].height,
            ),
            (4, 3, 20, 8),
        );
        assert_eq!(positioned[0].z_order, 7);
        assert!(positioned[0].visible);
        assert!(positioned[0].pinned);
        assert_eq!(positioned[0].opacity.to_bits(), 0.75_f32.to_bits());
        assert!(!positioned[0].is_focused);
        assert_eq!(mux.iter_panes().len(), 2);

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing_with_float(),
            None,
        )
        .expect("identical floating snapshot should be a no-op replay");
        let replayed = mux
            .get_pane(local_float_id)
            .expect("no-op replay should preserve the floating registration");
        assert!(Arc::ptr_eq(&replayed, &floating_pane));
        let replayed_positioned = tab.iter_floating_panes();
        assert_eq!(replayed_positioned.len(), 1);
        assert!(Arc::ptr_eq(&replayed_positioned[0].pane, &floating_pane));
        assert_eq!(mux.iter_panes().len(), 2);

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            None,
        )
        .expect("snapshot removal should retire the stale floating mirror");
        assert!(tab.iter_floating_panes().is_empty());
        assert!(mux.get_pane(local_float_id).is_none());
        assert_eq!(inner.remote_to_local_pane_id(&mux, 62), None);
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
        let _domain = register_test_client_domain(&mux, &inner);
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
    fn existing_remote_window_mapping_attaches_through_mux_authority_once() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);

        let local_domain_id = alloc_domain_id();
        let inner = test_client_inner(local_domain_id);
        let _domain = register_test_client_domain(&mux, &inner);
        let local_window_id = *mux.new_empty_window(Some("ops".to_string()), None);
        inner.record_remote_to_local_window_mapping(41, local_window_id);
        let observed_additions = Arc::new(Mutex::new(Vec::new()));
        let observed_additions_for_subscriber = Arc::clone(&observed_additions);
        mux.subscribe(move |notification| {
            if let MuxNotification::WindowTopologyChanged(change) = notification {
                for &(tab_id, window_id) in change.attached_tabs() {
                    if window_id != local_window_id {
                        continue;
                    }
                    lock_or_recover(&observed_additions_for_subscriber, "observed_tab_additions")
                        .push(tab_id);
                }
            }
            true
        })
        .expect("subscribe to canonical tab attachment events");

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            None,
        )
        .expect("existing remote window mapping should attach through mux authority");
        let local_tab_id = inner
            .remote_to_local_tab_id(51)
            .expect("remote tab should map locally");
        assert_eq!(
            *lock_or_recover(&observed_additions, "observed_tab_additions"),
            vec![local_tab_id],
        );

        ClientDomain::process_pane_list(
            &mux,
            Arc::clone(&inner),
            sample_remote_tab_listing(),
            None,
        )
        .expect("stable remote topology should not reattach its exact tab");
        assert_eq!(
            *lock_or_recover(&observed_additions, "observed_tab_additions"),
            vec![local_tab_id],
            "stable resync must not publish a duplicate tab attachment",
        );
        let local_tab = mux
            .get_tab(local_tab_id)
            .expect("mapped tab should remain registered");
        let attached_exactly_once = mux
            .get_window(local_window_id)
            .expect("mapped window should remain registered")
            .iter()
            .filter(|candidate| Arc::ptr_eq(candidate, &local_tab))
            .count();
        assert_eq!(attached_exactly_once, 1);
    }

    #[test]
    fn process_pane_list_keeps_workspace_mismatch_out_of_primary_window() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);

        let local_domain_id = alloc_domain_id();
        let inner = test_client_inner(local_domain_id);
        let _domain = register_test_client_domain(&mux, &inner);
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
        let _domain = register_test_client_domain(&mux, &inner);
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
            format!("{error:#}").contains("remote tab 51 didn't resolve after resync"),
            "unexpected error: {error:#}",
            error = error
        );
    }
}
